//! # floptle-script — the Lua scripting host (ADR-0003)
//!
//! Game logic lives in `.lua` files under a project's `scripts/` folder, attached
//! to nodes (the [`floptle_core::Scripts`] component names which scripts run, with
//! per-instance float `params`). [`ScriptHost`] embeds Lua (LuaJIT via `mlua`) and
//! drives them each frame.
//!
//! ## The script contract
//! A script file defines plain functions in its own sandboxed environment:
//! ```lua
//! defaults = { speed = 45 }              -- tunables shown in the Inspector
//!
//! function start(node) end               -- once, when play begins (optional)
//!
//! function update(node, dt)              -- every frame while playing
//!   node.yaw = node.yaw + math.rad(params.speed) * dt
//! end
//! ```
//! The host hands each call a mutable `node` table (`x/y/z`, `scale`/`scale_x..z`,
//! `yaw/pitch/roll` in radians) synced to the node's [`Transform`] before the call
//! and read back after, plus the globals `params` (this instance's values), `time`
//! (seconds since play started) and `dt`. The full Lua standard library is in
//! scope; `log("...")` prints to the engine console.
//!
//! Each `(node, script)` pair gets its own environment so per-instance state
//! persists across frames, and the host **hot-reloads** a script when its file
//! changes on disk (re-running it in a fresh environment).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;

use floptle_core::math::{DVec3, EulerRot, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_core::{Entity, Material, Matter, RigidBody, Scripts, Visible, World};
use mlua::{Function, Lua, RegistryKey, Table, Value, Variadic};

/// Severity of a captured script log line (the engine Console colors by this).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Warn,
    Error,
}

/// One line emitted by a running script — a `print`/`log` call or a raised error.
/// `source` is the originating `(script name, 1-based line)` when known, so the
/// editor's Console can jump to it.
#[derive(Clone, Debug)]
pub struct ScriptLog {
    pub level: LogLevel,
    pub msg: String,
    pub source: Option<(String, u32)>,
}

/// Parse the 1-based line out of an mlua error string (formatted `name:LINE: msg`).
fn error_line(msg: &str) -> u32 {
    msg.split(':').find_map(|s| s.trim().parse::<u32>().ok()).unwrap_or(0)
}

/// A snapshot of player input for one frame, fed to scripts via the `input` global
/// (so games can read the keyboard/mouse). Key names are lowercase
/// (`"w"`, `"space"`, `"left"`, `"escape"`, …). Mouse position is in pixels;
/// buttons are 0 = left, 1 = right, 2 = middle.
#[derive(Clone, Debug, Default)]
pub struct InputSnapshot {
    /// Keys currently held this frame.
    pub keys_down: std::collections::HashSet<String>,
    /// Keys that went down THIS frame (edge).
    pub keys_pressed: std::collections::HashSet<String>,
    /// Keys that went up THIS frame (edge).
    pub keys_released: std::collections::HashSet<String>,
    pub mouse: (f32, f32),
    pub mouse_delta: (f32, f32),
    pub scroll: f32,
    pub buttons_down: [bool; 3],
    pub buttons_pressed: [bool; 3],
}

/// A script source file's reload state: a generation that bumps whenever the file
/// changes, plus the last error seen for the current generation (so a broken
/// script is compiled at most once per edit, not re-run every frame).
struct Source {
    generation: u64,
    mtime: Option<SystemTime>,
    error: Option<String>,
}

/// A live `(node, script)` environment — the Lua table the script's functions
/// close over, tagged with the source generation it was built from.
struct Instance {
    env: RegistryKey,
    generation: u64,
    started: bool,
    seen: bool,
}

/// The pre-call `node` values, so we only write back fields the script changed
/// (avoids quat↔euler drift on untouched rotations, etc.).
struct NodePre {
    x: f64,
    y: f64,
    z: f64,
    sx: f64,
    sy: f64,
    sz: f64,
    scale: f64,
    yaw: f64,
    pitch: f64,
    roll: f64,
}

/// Embeds Lua and runs the scripts attached to a world's nodes.
pub struct ScriptHost {
    lua: Lua,
    sources: HashMap<String, Source>,
    instances: HashMap<(u32, String), Instance>,
    errors: Vec<String>,
    /// Captured `print`/`log` output (and errors) since the last drain — the editor
    /// Console reads these. Shared with the Lua `print`/`log` closures.
    logs: Rc<RefCell<Vec<ScriptLog>>>,
    /// This frame's player input, shared with the Lua `input` table's functions.
    input: Rc<RefCell<InputSnapshot>>,
    /// This frame's physics body state per entity index (velocity + grounded), fed in
    /// before `run` so scripts can read `node.vx/vy/vz/grounded`.
    bodies: Rc<RefCell<HashMap<u32, BodyState>>>,
    /// Velocities scripts wrote this frame (entity index → new velocity), drained by
    /// the editor and applied to the physics sim.
    body_changes: Rc<RefCell<HashMap<u32, [f32; 3]>>>,
    /// Capsule heights scripts wrote this frame (entity index → height), drained and
    /// applied to the sim — for crouching.
    body_height_changes: Rc<RefCell<HashMap<u32, f32>>>,
    /// The physics colliders for THIS frame, so `raycast(...)` works inside a script. The
    /// editor lends the sim's colliders before running scripts and takes them back after.
    colliders: Rc<RefCell<Vec<Box<dyn floptle_physics::CollisionShape>>>>,
    /// The scene graph mirror the node handles read/write (synced each `run`).
    scene: Rc<RefCell<SceneMirror>>,
    /// Live per-(entity, script) environments, for script handles.
    envs: Rc<RefCell<HashMap<(u32, String), Table>>>,
    /// Mesh model paths scripts wrote this frame (entity index → new asset path), applied
    /// to the ECS `Matter::Mesh` in `run` and drained by the editor to re-import the GPU mesh.
    model_changes: Rc<RefCell<HashMap<u32, String>>>,
    /// Material refs scripts assigned this frame (entity index → preset name / asset path),
    /// resolved against `materials` and applied to the ECS in `run`.
    material_changes: Rc<RefCell<HashMap<u32, String>>>,
    /// `node.visible = ...` writes (entity index → shown), applied as a `Visible` component.
    visible_changes: Rc<RefCell<HashMap<u32, bool>>>,
    /// `node:getcomponent(name).field = value` writes, flushed to the ECS after `run`.
    component_changes: Rc<RefCell<HashMap<(u32, String, String), f64>>>,
    /// The material presets the editor lends each frame (name → Material), so a script can
    /// set `node.material = "Gold"` (or an `assets.getFile("materials/Gold.ron")`).
    materials: Rc<RefCell<HashMap<String, Material>>>,
    /// The project root, so `assets.getFile` / `assets.getContents` can resolve paths the
    /// dev writes relative to it (the `Assets/` folder). Set by the editor each frame.
    project_root: Rc<RefCell<PathBuf>>,
    /// A pending mouse-lock request from `input.lockMouse()` / `input.unlockMouse()`:
    /// `Some(true)` = lock (grab + hide the cursor), `Some(false)` = unlock, `None` = no
    /// change this frame. The editor drains it after `run` and applies it to the window.
    mouse_lock: Rc<RefCell<Option<bool>>>,
}

/// A mirror of the scene graph the Lua node/script handles read and write, synced from
/// the ECS at the start of each `run` and flushed back at the end. It decouples the Lua
/// handles (which can persist across frames, e.g. a cached manager reference) from the
/// `&mut World` borrow, and lets one script reach any other node by hierarchy or name.
#[derive(Default)]
struct SceneMirror {
    /// Stable iteration order (entity index), for deterministic name lookups.
    order: Vec<u32>,
    names: HashMap<u32, String>,
    parent: HashMap<u32, u32>,
    children: HashMap<u32, Vec<u32>>,
    /// Entity → the script kinds attached to it (for `node:getscript`).
    scripts: HashMap<u32, Vec<String>>,
    /// Live transforms (read/written by node handles; flushed to the ECS after `run`).
    transforms: HashMap<u32, Transform>,
    /// Mesh nodes' current model path (so a script can read `node.model`).
    models: HashMap<u32, String>,
    /// Nodes that carry an explicit `Visible` component (so a script can read
    /// `node.visible`; absent = visible by default).
    visible: HashMap<u32, bool>,
    /// entity → component name → (field → value): the numeric fields scripts can read via
    /// `node:getcomponent("PointLight"/"RigidBody")`. Synced each run for read-back; writes
    /// go through `Shared::component_changes` and are flushed to the ECS after `run`.
    components: HashMap<u32, HashMap<String, HashMap<String, f64>>>,
    /// Entity index → its `Entity` (with generation), so handle-written transforms flush
    /// back to the right ECS entity.
    ents: HashMap<u32, Entity>,
    /// Entities whose transform a handle wrote this frame (so we only flush those back —
    /// the current node still flushes via the value-table path).
    dirty: std::collections::HashSet<u32>,
}

/// The interior-mutable state the Lua handle closures share with the host: the scene
/// mirror, the physics body bridges, and the per-(entity, script) environments.
#[derive(Clone)]
struct Shared {
    scene: Rc<RefCell<SceneMirror>>,
    bodies: Rc<RefCell<HashMap<u32, BodyState>>>,
    body_changes: Rc<RefCell<HashMap<u32, [f32; 3]>>>,
    body_height_changes: Rc<RefCell<HashMap<u32, f32>>>,
    /// (entity index, script kind) → that instance's live Lua environment table, so a
    /// script handle can read its state, call its methods, and read its params.
    envs: Rc<RefCell<HashMap<(u32, String), Table>>>,
    /// `node.model = ...` writes (entity index → asset path), applied to `Matter::Mesh`.
    model_changes: Rc<RefCell<HashMap<u32, String>>>,
    /// `node.material = ...` writes (entity index → preset name / asset path).
    material_changes: Rc<RefCell<HashMap<u32, String>>>,
    /// `node.visible = ...` writes (entity index → shown), applied as a `Visible` component.
    visible_changes: Rc<RefCell<HashMap<u32, bool>>>,
    /// `node:getcomponent(name).field = value` writes: (entity, component, field) → number,
    /// flushed to the ECS after `run` (and read back the same frame).
    component_changes: Rc<RefCell<HashMap<(u32, String, String), f64>>>,
}

/// A physics body's state exposed to its node's scripts.
#[derive(Clone, Copy, Debug)]
pub struct BodyState {
    pub vel: [f32; 3],
    /// The body's "up" (−gravity) — Y for normal gravity, radial on a planet. Lets a
    /// controller script move along the surface and jump correctly on any world.
    pub up: [f32; 3],
    pub grounded: bool,
    /// Current capsule standing height — a controller reads it and writes `node.height`
    /// to crouch (the engine resizes the capsule, feet planted).
    pub height: f32,
}

impl Default for BodyState {
    fn default() -> Self {
        Self { vel: [0.0; 3], up: [0.0, 1.0, 0.0], grounded: false, height: 2.0 }
    }
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
        // `raycast(ox,oy,oz, dx,dy,dz, max)` against the world's colliders (terrain +
        // mesh): returns a hit table {x,y,z, nx,ny,nz, distance} or nil. Use it for ground
        // checks, line-of-sight, shooting.
        let colliders: Rc<RefCell<Vec<Box<dyn floptle_physics::CollisionShape>>>> =
            Rc::new(RefCell::new(Vec::new()));
        {
            let cols = colliders.clone();
            type Args = (f64, f64, f64, f64, f64, f64, f64);
            if let Ok(f) = lua.create_function(move |lua, (ox, oy, oz, dx, dy, dz, max): Args| {
                let hit = floptle_physics::raycast_colliders(
                    &cols.borrow(),
                    glam::Vec3::new(ox as f32, oy as f32, oz as f32),
                    glam::Vec3::new(dx as f32, dy as f32, dz as f32),
                    max as f32,
                );
                match hit {
                    Some(h) => {
                        let t = lua.create_table()?;
                        t.set("x", h.point[0] as f64)?;
                        t.set("y", h.point[1] as f64)?;
                        t.set("z", h.point[2] as f64)?;
                        t.set("nx", h.normal[0] as f64)?;
                        t.set("ny", h.normal[1] as f64)?;
                        t.set("nz", h.normal[2] as f64)?;
                        t.set("distance", h.distance as f64)?;
                        Ok(Value::Table(t))
                    }
                    None => Ok(Value::Nil),
                }
            }) {
                let _ = lua.globals().set("raycast", f);
            }
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
        };
        if let Err(e) = install_handle_api(&lua, &shared) {
            eprintln!("[lua] failed to install the node/script reference API: {e}");
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
            scene: shared.scene.clone(),
            envs: shared.envs.clone(),
            model_changes: shared.model_changes.clone(),
            material_changes: shared.material_changes.clone(),
            visible_changes: shared.visible_changes.clone(),
            component_changes: shared.component_changes.clone(),
            materials: Rc::new(RefCell::new(HashMap::new())),
            project_root,
            mouse_lock,
        }
    }

    /// Drain a pending `input.lockMouse()` / `input.unlockMouse()` request from this frame:
    /// `Some(true)` = lock (grab + hide cursor), `Some(false)` = unlock, `None` = unchanged.
    pub fn take_mouse_lock(&self) -> Option<bool> {
        self.mouse_lock.borrow_mut().take()
    }

    /// Lend the sim's colliders to the script host for one frame so `raycast(...)` can see
    /// them (the editor takes them back with [`take_colliders`](Self::take_colliders)
    /// before stepping physics). Call before [`run`](Self::run).
    pub fn set_colliders(&self, cols: Vec<Box<dyn floptle_physics::CollisionShape>>) {
        *self.colliders.borrow_mut() = cols;
    }

    /// Reclaim the colliders lent via [`set_colliders`](Self::set_colliders). Call after
    /// [`run`](Self::run), before stepping the sim.
    pub fn take_colliders(&self) -> Vec<Box<dyn floptle_physics::CollisionShape>> {
        std::mem::take(&mut self.colliders.borrow_mut())
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
        for (e, scripts) in &work {
            let Some(mut tr) = world.get::<Transform>(*e).copied() else { continue };
            let tr0 = tr; // frame-start, to detect a self-move via the `node` argument
            let mut ran = false;
            for inst in &scripts.0 {
                if inst.enabled {
                    self.tick_instance(*e, &inst.kind, &inst.params, &mut tr, dt, time);
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
        // Flush transforms that a handle wrote on OTHER nodes back to the ECS.
        self.flush_scene(world);
        // Apply script-driven component swaps: mesh model + material. (Model paths stay in
        // `model_changes` for the editor to drain and re-import the GPU mesh; materials are
        // resolved here against the lent preset map and applied directly.)
        {
            let scene = self.scene.borrow();
            for (eid, path) in self.model_changes.borrow().iter() {
                if let Some(&ent) = scene.ents.get(eid) {
                    if let Some(Matter::Mesh { asset_path }) = world.get_mut::<Matter>(ent) {
                        *asset_path = path.clone();
                    }
                }
            }
            let mats = self.materials.borrow();
            for (eid, refstr) in self.material_changes.borrow().iter() {
                if let Some(&ent) = scene.ents.get(eid) {
                    if let Some(m) = mats.get(&material_key(refstr)) {
                        world.insert(ent, m.clone());
                    }
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

        // Drop environments whose (node, script) no longer exists.
        let stale: Vec<(u32, String)> =
            self.instances.iter().filter(|(_, i)| !i.seen).map(|(k, _)| k.clone()).collect();
        for k in stale {
            if let Some(inst) = self.instances.remove(&k) {
                let _ = self.lua.remove_registry_value(inst.env);
            }
            self.envs.borrow_mut().remove(&k);
        }
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
            if let (Some(&ent), Some(tr)) = (s.ents.get(&id), s.transforms.get(&id)) {
                if let Some(slot) = world.get_mut::<Transform>(ent) {
                    *slot = *tr;
                }
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
            match build_env(&self.lua, &src, name) {
                Ok(env) => {
                    if let Some(old) = self.instances.remove(&key) {
                        let _ = self.lua.remove_registry_value(old.env);
                    }
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

    /// Run one already-ensured `(entity, script)` instance's lifecycle for this frame.
    fn tick_instance(
        &mut self,
        e: Entity,
        name: &str,
        params: &[(String, f32)],
        tr: &mut Transform,
        dt: f32,
        time: f32,
    ) {
        let key = (e.index(), name.to_string());
        let (first, env) = {
            let Some(inst) = self.instances.get_mut(&key) else { return };
            let first = !inst.started;
            inst.started = true;
            let Ok(env) = self.lua.registry_value::<Table>(&inst.env) else { return };
            (first, env)
        };
        let eid = e.index();
        let body = self.bodies.borrow().get(&eid).copied();
        if let Err(err) = self.tick(&env, params, tr, dt, time, first, eid, body) {
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
    ) -> mlua::Result<()> {
        env.set("params", params_table(&self.lua, env, params)?)?;
        env.set("time", time as f64)?;
        env.set("dt", dt as f64)?;

        let node = node_table(&self.lua, eid, tr, body)?;
        let pre = node_pre(tr);
        // Prefer the short hook names (`start`/`update`); `on_start`/`on_update`
        // still work for older scripts.
        if first {
            if let Some(f) = lifecycle_fn(env, &["start", "on_start"])? {
                f.call::<()>(node.clone())?;
            }
        }
        if let Some(f) = lifecycle_fn(env, &["update", "on_update"])? {
            f.call::<()>((node.clone(), dt as f64))?;
        }
        // Read back the (possibly script-modified) velocity + height for a physics body.
        if let Some(b) = body {
            let vx: f64 = node.get("vx").unwrap_or(0.0);
            let vy: f64 = node.get("vy").unwrap_or(0.0);
            let vz: f64 = node.get("vz").unwrap_or(0.0);
            self.body_changes.borrow_mut().insert(eid, [vx as f32, vy as f32, vz as f32]);
            let h: f64 = node.get("height").unwrap_or(b.height as f64);
            self.body_height_changes.borrow_mut().insert(eid, h as f32);
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

/// If `b[j]` opens a Lua long bracket (`[`, then `=`×level, then `[`), return its
/// level. Used to copy long strings/comments verbatim.
fn long_bracket_level(b: &[u8], j: usize) -> Option<usize> {
    if b.get(j) != Some(&b'[') {
        return None;
    }
    let mut k = j + 1;
    let mut level = 0;
    while b.get(k) == Some(&b'=') {
        level += 1;
        k += 1;
    }
    if b.get(k) == Some(&b'[') { Some(level) } else { None }
}

/// Copy a long bracket (string or comment) of the given `level` starting at `j`
/// (where `b[j] == '['`) into `out`, through its matching close. Returns the index
/// just past the close (or end of input if unterminated).
fn copy_long_bracket(b: &[u8], mut j: usize, level: usize, out: &mut Vec<u8>) -> usize {
    let span = 2 + level; // '[' + '='*level + '['  (and likewise for the closer)
    for _ in 0..span {
        if j < b.len() {
            out.push(b[j]);
            j += 1;
        }
    }
    while j < b.len() {
        if b[j] == b']' {
            let mut k = j + 1;
            let mut cnt = 0;
            while b.get(k) == Some(&b'=') {
                cnt += 1;
                k += 1;
            }
            if cnt == level && b.get(k) == Some(&b']') {
                for _ in 0..span {
                    out.push(b[j]);
                    j += 1;
                }
                return j;
            }
        }
        out.push(b[j]);
        j += 1;
    }
    j
}

/// Walk backward over already-emitted `out` from `end` to the start of the lvalue
/// the compound operator applies to: a name, dotted field chain (`a.b.c`), or
/// index chain (`a[i]`, `t[k].x`, nested brackets balanced). Stops at the first
/// byte that can't be part of an lvalue (whitespace, `=`, `(`, a keyword boundary…).
fn lvalue_start(out: &[u8], end: usize) -> usize {
    let mut j = end;
    while j > 0 && matches!(out[j - 1], b' ' | b'\t') {
        j -= 1;
    }
    loop {
        if j == 0 {
            break;
        }
        let c = out[j - 1];
        if c == b']' {
            // Balance back to the matching '['.
            let mut depth = 0;
            while j > 0 {
                let d = out[j - 1];
                j -= 1;
                if d == b']' {
                    depth += 1;
                } else if d == b'[' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
            continue;
        }
        if c == b')' {
            // Balance back to the matching '(' — a call/parenthesized receiver, so
            // `f().x` / `(a).b` are captured whole rather than just the trailing field.
            let mut depth = 0;
            while j > 0 {
                let d = out[j - 1];
                j -= 1;
                if d == b')' {
                    depth += 1;
                } else if d == b'(' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
            continue;
        }
        if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b':' {
            j -= 1;
            continue;
        }
        break;
    }
    j
}

/// Rewrite Lua compound-assignment operators (`+= -= *= /= %= ^= ..=`) — which
/// Lua 5.1 / LuaJIT do NOT support — into plain assignments before compiling, e.g.
/// `x += y` → `x = x + (y)`, `t.k *= a + b` → `t.k = t.k * (a + b)`. A single-pass
/// scanner skips strings and comments (so `"a += b"` and `-- a += b` are untouched)
/// and adds NO newlines, so error line numbers stay correct. The `(R)` parentheses
/// preserve precedence.
fn preprocess(src: &str) -> String {
    let b = src.as_bytes();
    let n = b.len();
    let mut out: Vec<u8> = Vec::with_capacity(n + 16);
    let mut i = 0;
    let mut pending_close = false; // inside a rewritten RHS — emit ')' at statement end
    while i < n {
        let c = b[i];

        // Line / long comment.
        if c == b'-' && b.get(i + 1) == Some(&b'-') {
            // A comment can't be part of a rewritten RHS — close it first, so
            // `x += 1 -- note` becomes `x = x + (1) -- note`, not `(1 -- note)`.
            if pending_close {
                while out.last().is_some_and(|&p| p == b' ' || p == b'\t') {
                    out.pop();
                }
                out.push(b')');
                out.push(b' ');
                pending_close = false;
            }
            out.push(b'-');
            out.push(b'-');
            i += 2;
            if let Some(level) = long_bracket_level(b, i) {
                i = copy_long_bracket(b, i, level, &mut out);
            } else {
                while i < n && b[i] != b'\n' {
                    out.push(b[i]);
                    i += 1;
                }
            }
            continue;
        }

        // Short string.
        if c == b'"' || c == b'\'' {
            out.push(c);
            i += 1;
            while i < n {
                let d = b[i];
                out.push(d);
                i += 1;
                if d == b'\\' && i < n {
                    out.push(b[i]);
                    i += 1;
                    continue;
                }
                if d == c || d == b'\n' {
                    break;
                }
            }
            continue;
        }

        // Long string.
        if c == b'[' {
            if let Some(level) = long_bracket_level(b, i) {
                i = copy_long_bracket(b, i, level, &mut out);
                continue;
            }
        }

        // Statement terminator — close any pending rewritten RHS.
        if c == b'\n' || c == b';' {
            if pending_close {
                out.push(b')');
                pending_close = false;
            }
            out.push(c);
            i += 1;
            continue;
        }

        // A block-ending or statement-introducing keyword also terminates a rewritten
        // RHS (the `end` in `if c then x += 1 end`, or the `return` in
        // `function f() x += 1 return x end`). These are reserved words that can't be
        // part of the expression, so close the paren before copying the keyword.
        // (`function` is excluded — it can begin an anonymous-function expression.)
        if pending_close && (c.is_ascii_alphabetic() || c == b'_') {
            let prev_ident = out.last().is_some_and(|&p| p.is_ascii_alphanumeric() || p == b'_');
            if !prev_ident {
                let mut k = i;
                while k < n && (b[k].is_ascii_alphanumeric() || b[k] == b'_') {
                    k += 1;
                }
                let word = std::str::from_utf8(&b[i..k]).unwrap_or("");
                if matches!(
                    word,
                    "end" | "else"
                        | "elseif"
                        | "then"
                        | "do"
                        | "until"
                        | "return"
                        | "local"
                        | "break"
                        | "goto"
                        | "if"
                        | "while"
                        | "for"
                        | "repeat"
                ) {
                    while out.last().is_some_and(|&p| p == b' ' || p == b'\t') {
                        out.pop();
                    }
                    out.push(b')');
                    out.push(b' ');
                    pending_close = false;
                }
            }
        }

        // Compound single-char ops: + - * / % ^  followed by '=' (but not "==").
        if !pending_close
            && matches!(c, b'+' | b'-' | b'*' | b'/' | b'%' | b'^')
            && b.get(i + 1) == Some(&b'=')
            && b.get(i + 2) != Some(&b'=')
        {
            let start = lvalue_start(&out, out.len());
            let lhs = std::str::from_utf8(&out[start..]).unwrap_or("").trim().to_string();
            if !lhs.is_empty() {
                out.extend_from_slice(b"= ");
                out.extend_from_slice(lhs.as_bytes());
                out.push(b' ');
                out.push(c);
                out.extend_from_slice(b" (");
                pending_close = true;
                i += 2;
                while i < n && matches!(b[i], b' ' | b'\t') {
                    i += 1;
                }
                continue;
            }
        }

        // Compound concat: ..=  (but not the start of a longer run).
        if !pending_close
            && c == b'.'
            && b.get(i + 1) == Some(&b'.')
            && b.get(i + 2) == Some(&b'=')
            && b.get(i + 3) != Some(&b'=')
        {
            let start = lvalue_start(&out, out.len());
            let lhs = std::str::from_utf8(&out[start..]).unwrap_or("").trim().to_string();
            if !lhs.is_empty() {
                out.extend_from_slice(b"= ");
                out.extend_from_slice(lhs.as_bytes());
                out.extend_from_slice(b" .. (");
                pending_close = true;
                i += 3;
                while i < n && matches!(b[i], b' ' | b'\t') {
                    i += 1;
                }
                continue;
            }
        }

        out.push(c);
        i += 1;
    }
    if pending_close {
        out.push(b')');
    }
    String::from_utf8(out).unwrap_or_else(|_| src.to_string())
}

/// Build a fresh sandbox environment for a script: a table whose metatable falls
/// through to the real globals (so `math`, `string`, `log`, … are in scope) while
/// the script's own assignments stay local. Running the chunk defines its
/// functions (`start`, `update`) in that table.
fn build_env(lua: &Lua, src: &str, name: &str) -> mlua::Result<Table> {
    let env = lua.create_table()?;
    let mt = lua.create_table()?;
    mt.set("__index", lua.globals())?;
    env.set_metatable(Some(mt));
    lua.load(&preprocess(src)).set_name(name).set_environment(env.clone()).exec()?;
    Ok(env)
}

/// The first of `names` that's a function in `env` (lets a hook have aliases).
fn lifecycle_fn(env: &Table, names: &[&str]) -> mlua::Result<Option<Function>> {
    for n in names {
        if let Value::Function(f) = env.get::<Value>(*n)? {
            return Ok(Some(f));
        }
    }
    Ok(None)
}

/// Build the `params` table a script sees: its declared `defaults` as the base, with
/// any per-instance overrides (Inspector tweaks) layered on top. Seeding from `defaults`
/// is what makes `params.foo` resolve out of the box — without it, a script with no saved
/// overrides sees an empty `params` and every `params.foo` reads `nil`.
fn params_table(lua: &Lua, env: &Table, params: &[(String, f32)]) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    if let Ok(defaults) = env.get::<Table>("defaults") {
        for (k, v) in defaults.pairs::<Value, Value>().flatten() {
            t.set(k, v)?;
        }
    }
    for (k, v) in params {
        t.set(k.as_str(), *v as f64)?;
    }
    Ok(t)
}

fn node_table(lua: &Lua, eid: u32, tr: &Transform, body: Option<BodyState>) -> mlua::Result<Table> {
    let (yaw, pitch, roll) = tr.rotation.to_euler(EulerRot::YXZ);
    let t = lua.create_table()?;
    // Tag with the entity + the node metatable so `node.parent`, `node:getscript(...)`,
    // `node:children()` etc. work. The transform fields below are direct table values, so
    // they're read/written normally (the metatable only supplies the missing keys).
    t.raw_set("__id", eid)?;
    if let Ok(mt) = lua.named_registry_value::<Table>("floptle_node_mt") {
        t.set_metatable(Some(mt));
    }
    // raw_set so these stay DIRECT table fields (not routed through the node metatable's
    // __newindex, which is for handles to other nodes) — the existing read-back path reads
    // them directly after `update`.
    t.raw_set("x", tr.translation.x)?;
    t.raw_set("y", tr.translation.y)?;
    t.raw_set("z", tr.translation.z)?;
    t.raw_set("scale_x", tr.scale.x as f64)?;
    t.raw_set("scale_y", tr.scale.y as f64)?;
    t.raw_set("scale_z", tr.scale.z as f64)?;
    t.raw_set("scale", tr.scale.x as f64)?; // uniform-scale shortcut
    t.raw_set("yaw", yaw as f64)?;
    t.raw_set("pitch", pitch as f64)?;
    t.raw_set("roll", roll as f64)?;
    // Physics body fields (present only on rigidbody nodes): read grounded, read/write
    // the velocity. The engine reads vx/vy/vz back after `update` and applies them.
    if let Some(b) = body {
        t.raw_set("vx", b.vel[0] as f64)?;
        t.raw_set("vy", b.vel[1] as f64)?;
        t.raw_set("vz", b.vel[2] as f64)?;
        t.raw_set("up_x", b.up[0] as f64)?;
        t.raw_set("up_y", b.up[1] as f64)?;
        t.raw_set("up_z", b.up[2] as f64)?;
        t.raw_set("grounded", b.grounded)?;
        t.raw_set("height", b.height as f64)?; // write to crouch (capsule resizes, feet planted)
    }
    Ok(t)
}

fn node_pre(tr: &Transform) -> NodePre {
    let (yaw, pitch, roll) = tr.rotation.to_euler(EulerRot::YXZ);
    NodePre {
        x: tr.translation.x,
        y: tr.translation.y,
        z: tr.translation.z,
        sx: tr.scale.x as f64,
        sy: tr.scale.y as f64,
        sz: tr.scale.z as f64,
        scale: tr.scale.x as f64,
        yaw: yaw as f64,
        pitch: pitch as f64,
        roll: roll as f64,
    }
}

/// Read the `node` table back into the Transform, writing only the fields the
/// script actually changed. `node.scale` (uniform) wins over per-axis if touched.
fn apply_node(t: &Table, tr: &mut Transform, pre: &NodePre) -> mlua::Result<()> {
    let x: f64 = t.get("x")?;
    let y: f64 = t.get("y")?;
    let z: f64 = t.get("z")?;
    if x != pre.x || y != pre.y || z != pre.z {
        tr.translation = DVec3::new(x, y, z);
    }

    let scale: f64 = t.get("scale")?;
    if scale != pre.scale {
        tr.scale = Vec3::splat(scale as f32);
    } else {
        let sx: f64 = t.get("scale_x")?;
        let sy: f64 = t.get("scale_y")?;
        let sz: f64 = t.get("scale_z")?;
        if sx != pre.sx || sy != pre.sy || sz != pre.sz {
            tr.scale = Vec3::new(sx as f32, sy as f32, sz as f32);
        }
    }

    let yaw: f64 = t.get("yaw")?;
    let pitch: f64 = t.get("pitch")?;
    let roll: f64 = t.get("roll")?;
    if yaw != pre.yaw || pitch != pre.pitch || roll != pre.roll {
        tr.rotation = Quat::from_euler(EulerRot::YXZ, yaw as f32, pitch as f32, roll as f32);
    }
    Ok(())
}

/// Create a Lua **node handle** for entity index `e`: a table `{__id = e}` with the shared
/// node metatable, so `h.x`, `h.parent`, `h:getscript("foo")`, etc. work for any node.
fn new_node_handle(lua: &Lua, e: u32) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.raw_set("__id", e)?;
    if let Ok(mt) = lua.named_registry_value::<Table>("floptle_node_mt") {
        t.set_metatable(Some(mt));
    }
    Ok(t)
}

/// Create a Lua **component handle** for component `comp` on entity index `e`: a table
/// `{__id, __comp}` with the shared component metatable, so `h.field` reads and
/// `h.field = value` records a write (flushed to the ECS after the frame).
fn new_component_handle(lua: &Lua, e: u32, comp: &str) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.raw_set("__id", e)?;
    t.raw_set("__comp", comp.to_string())?;
    if let Ok(mt) = lua.named_registry_value::<Table>("floptle_component_mt") {
        t.set_metatable(Some(mt));
    }
    Ok(t)
}

/// Create a Lua **script handle** for script `name` on entity index `e`: a table
/// `{__id, __script}` with the shared script metatable, so you can read/write its state,
/// call its methods, and reach `.node` / `.params`.
fn new_script_handle(lua: &Lua, e: u32, name: &str) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.raw_set("__id", e)?;
    t.raw_set("__script", name)?;
    if let Ok(mt) = lua.named_registry_value::<Table>("floptle_script_mt") {
        t.set_metatable(Some(mt));
    }
    Ok(t)
}

fn as_num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => Some(*n),
        Value::Integer(n) => Some(*n as f64),
        _ => None,
    }
}

/// The preset name a `node.material = ...` ref resolves to: the file stem of a path
/// (`"assets/materials/Gold.ron"` → `"Gold"`) or the bare name as given (`"Gold"`).
fn material_key(refstr: &str) -> String {
    Path::new(refstr)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| refstr.to_string())
}

/// The numeric component fields exposed to scripts via `node:getcomponent(name)`, mirrored
/// from the live ECS each frame. Extend here (and in [`apply_component_field`]) to reach
/// more components / fields.
fn mirror_components(world: &World, e: Entity) -> HashMap<String, HashMap<String, f64>> {
    let mut out: HashMap<String, HashMap<String, f64>> = HashMap::new();
    if let Some(Matter::PointLight { color, intensity, range }) = world.get::<Matter>(e) {
        out.insert(
            "PointLight".to_string(),
            HashMap::from([
                ("intensity".to_string(), *intensity as f64),
                ("range".to_string(), *range as f64),
                ("r".to_string(), color[0] as f64),
                ("g".to_string(), color[1] as f64),
                ("b".to_string(), color[2] as f64),
            ]),
        );
    }
    if let Some(rb) = world.get::<RigidBody>(e) {
        out.insert(
            "RigidBody".to_string(),
            HashMap::from([
                ("friction".to_string(), rb.friction as f64),
                ("restitution".to_string(), rb.restitution as f64),
                ("gravity".to_string(), if rb.gravity { 1.0 } else { 0.0 }),
                ("radius".to_string(), rb.radius as f64),
                ("height".to_string(), rb.height as f64),
            ]),
        );
    }
    out
}

/// Apply a `node:getcomponent(name).field = value` write back to the ECS (mirror of
/// [`mirror_components`]). Unknown component/field names are ignored.
fn apply_component_field(world: &mut World, ent: Entity, comp: &str, field: &str, val: f64) {
    match comp {
        "PointLight" => {
            if let Some(Matter::PointLight { color, intensity, range }) = world.get_mut::<Matter>(ent) {
                match field {
                    "intensity" => *intensity = val as f32,
                    "range" => *range = val as f32,
                    "r" => color[0] = val as f32,
                    "g" => color[1] = val as f32,
                    "b" => color[2] = val as f32,
                    _ => {}
                }
            }
        }
        "RigidBody" => {
            if let Some(rb) = world.get_mut::<RigidBody>(ent) {
                match field {
                    "friction" => rb.friction = val as f32,
                    "restitution" => rb.restitution = val as f32,
                    "gravity" => rb.gravity = val != 0.0,
                    "radius" => rb.radius = val as f32,
                    "height" => rb.height = val as f32,
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Install the cross-node / cross-script reference layer into the Lua state: the `node`
/// and `script` metatables (transform/body access + hierarchy traversal + method/state
/// access) and the `find` / `findAll` / `findScript` globals. The handle closures share
/// the scene mirror + body bridges + env map via `shared`.
fn install_handle_api(lua: &Lua, shared: &Shared) -> mlua::Result<()> {
    // ---- node metatable -------------------------------------------------------------
    let node_mt = lua.create_table()?;
    {
        let scene = shared.scene.clone();
        let bodies = shared.bodies.clone();
        let body_changes = shared.body_changes.clone();
        let idx = lua.create_function(move |lua, (this, key): (Table, String)| {
            let e: u32 = this.raw_get("__id")?;
            // Transform reads.
            {
                let s = scene.borrow();
                if let Some(tr) = s.transforms.get(&e) {
                    match key.as_str() {
                        "x" => return Ok(Value::Number(tr.translation.x)),
                        "y" => return Ok(Value::Number(tr.translation.y)),
                        "z" => return Ok(Value::Number(tr.translation.z)),
                        "scale" | "scale_x" => return Ok(Value::Number(tr.scale.x as f64)),
                        "scale_y" => return Ok(Value::Number(tr.scale.y as f64)),
                        "scale_z" => return Ok(Value::Number(tr.scale.z as f64)),
                        "yaw" | "pitch" | "roll" => {
                            let (y, p, r) = tr.rotation.to_euler(EulerRot::YXZ);
                            let v = match key.as_str() {
                                "yaw" => y,
                                "pitch" => p,
                                _ => r,
                            };
                            return Ok(Value::Number(v as f64));
                        }
                        _ => {}
                    }
                }
            }
            // Identity / hierarchy fields.
            match key.as_str() {
                "id" => return Ok(Value::Integer(e as i64)),
                "name" => {
                    let n = scene.borrow().names.get(&e).cloned();
                    return Ok(match n {
                        Some(n) => Value::String(lua.create_string(&n)?),
                        None => Value::Nil,
                    });
                }
                "valid" => return Ok(Value::Boolean(scene.borrow().transforms.contains_key(&e))),
                "parent" => {
                    let p = scene.borrow().parent.get(&e).copied();
                    return Ok(match p {
                        Some(p) => Value::Table(new_node_handle(lua, p)?),
                        None => Value::Nil,
                    });
                }
                // The mesh node's current model path (nil on non-mesh nodes). Assigning it
                // (see __newindex) swaps the model at runtime.
                "model" => {
                    let m = scene.borrow().models.get(&e).cloned();
                    return Ok(match m {
                        Some(p) => Value::String(lua.create_string(&p)?),
                        None => Value::Nil,
                    });
                }
                // Whether the node's geometry is drawn (true unless explicitly hidden).
                "visible" => {
                    let v = scene.borrow().visible.get(&e).copied().unwrap_or(true);
                    return Ok(Value::Boolean(v));
                }
                _ => {}
            }
            // Physics body fields.
            match key.as_str() {
                "vx" | "vy" | "vz" => {
                    let vel = body_changes
                        .borrow()
                        .get(&e)
                        .copied()
                        .or_else(|| bodies.borrow().get(&e).map(|b| b.vel));
                    return Ok(match vel {
                        Some(v) => Value::Number(match key.as_str() {
                            "vx" => v[0],
                            "vy" => v[1],
                            _ => v[2],
                        } as f64),
                        None => Value::Nil,
                    });
                }
                "up_x" | "up_y" | "up_z" => {
                    return Ok(match bodies.borrow().get(&e) {
                        Some(b) => Value::Number(match key.as_str() {
                            "up_x" => b.up[0],
                            "up_y" => b.up[1],
                            _ => b.up[2],
                        } as f64),
                        None => Value::Nil,
                    });
                }
                "grounded" => {
                    return Ok(Value::Boolean(
                        bodies.borrow().get(&e).map(|b| b.grounded).unwrap_or(false),
                    ));
                }
                "height" => {
                    return Ok(match bodies.borrow().get(&e) {
                        Some(b) => Value::Number(b.height as f64),
                        None => Value::Nil,
                    });
                }
                _ => {}
            }
            // Otherwise a method (children / getchild / getscript / find …) or nil.
            let methods: Table = lua.named_registry_value("floptle_node_methods")?;
            methods.get::<Value>(key)
        })?;
        node_mt.set("__index", idx)?;
    }
    {
        let scene = shared.scene.clone();
        let bodies = shared.bodies.clone();
        let body_changes = shared.body_changes.clone();
        let body_height = shared.body_height_changes.clone();
        let model_changes = shared.model_changes.clone();
        let material_changes = shared.material_changes.clone();
        let visible_changes = shared.visible_changes.clone();
        let newidx = lua.create_function(move |_, (this, key, val): (Table, String, Value)| {
            let e: u32 = this.raw_get("__id")?;
            // Transform writes.
            {
                let mut s = scene.borrow_mut();
                if let Some(tr) = s.transforms.get_mut(&e) {
                    let mut handled = true;
                    match key.as_str() {
                        "x" => {
                            if let Some(n) = as_num(&val) {
                                tr.translation.x = n;
                            }
                        }
                        "y" => {
                            if let Some(n) = as_num(&val) {
                                tr.translation.y = n;
                            }
                        }
                        "z" => {
                            if let Some(n) = as_num(&val) {
                                tr.translation.z = n;
                            }
                        }
                        "scale" => {
                            if let Some(n) = as_num(&val) {
                                tr.scale = Vec3::splat(n as f32);
                            }
                        }
                        "scale_x" => {
                            if let Some(n) = as_num(&val) {
                                tr.scale.x = n as f32;
                            }
                        }
                        "scale_y" => {
                            if let Some(n) = as_num(&val) {
                                tr.scale.y = n as f32;
                            }
                        }
                        "scale_z" => {
                            if let Some(n) = as_num(&val) {
                                tr.scale.z = n as f32;
                            }
                        }
                        "yaw" | "pitch" | "roll" => {
                            if let Some(n) = as_num(&val) {
                                let (mut y, mut p, mut r) = tr.rotation.to_euler(EulerRot::YXZ);
                                let changed = match key.as_str() {
                                    "yaw" => n != y as f64,
                                    "pitch" => n != p as f64,
                                    _ => n != r as f64,
                                };
                                if changed {
                                    match key.as_str() {
                                        "yaw" => y = n as f32,
                                        "pitch" => p = n as f32,
                                        _ => r = n as f32,
                                    }
                                    tr.rotation = Quat::from_euler(EulerRot::YXZ, y, p, r);
                                }
                            }
                        }
                        _ => handled = false,
                    }
                    if handled {
                        s.dirty.insert(e);
                        return Ok(());
                    }
                }
            }
            // Physics body writes.
            match key.as_str() {
                "vx" | "vy" | "vz" => {
                    if let Some(n) = as_num(&val) {
                        let mut bc = body_changes.borrow_mut();
                        let mut v = bc
                            .get(&e)
                            .copied()
                            .or_else(|| bodies.borrow().get(&e).map(|b| b.vel))
                            .unwrap_or([0.0; 3]);
                        match key.as_str() {
                            "vx" => v[0] = n as f32,
                            "vy" => v[1] = n as f32,
                            _ => v[2] = n as f32,
                        }
                        bc.insert(e, v);
                    }
                    return Ok(());
                }
                "height" => {
                    if let Some(n) = as_num(&val) {
                        body_height.borrow_mut().insert(e, n as f32);
                    }
                    return Ok(());
                }
                _ => {}
            }
            // Component swaps (applied to the ECS at the end of `run`): the mesh model path
            // and a material (preset name or `assets.getFile("materials/X.ron")`).
            match key.as_str() {
                "model" => {
                    if let Value::String(s) = &val {
                        model_changes.borrow_mut().insert(e, s.to_string_lossy().to_string());
                    }
                    return Ok(());
                }
                "material" => {
                    if let Value::String(s) = &val {
                        material_changes.borrow_mut().insert(e, s.to_string_lossy().to_string());
                    }
                    return Ok(());
                }
                "visible" => {
                    if let Value::Boolean(b) = val {
                        visible_changes.borrow_mut().insert(e, b);
                    }
                    return Ok(());
                }
                _ => {}
            }
            // Unknown key: stash it on the handle table (harmless; lets scripts tag nodes).
            this.raw_set(key, val)?;
            Ok(())
        })?;
        node_mt.set("__newindex", newidx)?;
    }
    lua.set_named_registry_value("floptle_node_mt", node_mt)?;

    // ---- component handle metatable (node:getcomponent) -----------------------------
    // A component handle reads its numeric fields from the mirror (or this frame's pending
    // writes) and records assignments; the writes are flushed to the ECS after `run`.
    {
        let comp_mt = lua.create_table()?;
        {
            let scene = shared.scene.clone();
            let changes = shared.component_changes.clone();
            let idx = lua.create_function(move |_, (this, key): (Table, String)| {
                let e: u32 = this.raw_get("__id")?;
                let comp: String = this.raw_get("__comp")?;
                if let Some(v) = changes.borrow().get(&(e, comp.clone(), key.clone())) {
                    return Ok(Value::Number(*v));
                }
                let s = scene.borrow();
                if let Some(v) =
                    s.components.get(&e).and_then(|c| c.get(&comp)).and_then(|m| m.get(&key))
                {
                    return Ok(Value::Number(*v));
                }
                Ok(Value::Nil)
            })?;
            comp_mt.set("__index", idx)?;
        }
        {
            let changes = shared.component_changes.clone();
            let newidx = lua.create_function(move |_, (this, key, val): (Table, String, Value)| {
                let e: u32 = this.raw_get("__id")?;
                let comp: String = this.raw_get("__comp")?;
                let n = match val {
                    Value::Number(n) => n,
                    Value::Integer(n) => n as f64,
                    Value::Boolean(b) => {
                        if b {
                            1.0
                        } else {
                            0.0
                        }
                    }
                    _ => {
                        return Err(mlua::Error::RuntimeError(format!(
                            "component field '{key}' must be a number"
                        )))
                    }
                };
                changes.borrow_mut().insert((e, comp, key), n);
                Ok(())
            })?;
            comp_mt.set("__newindex", newidx)?;
        }
        lua.set_named_registry_value("floptle_component_mt", comp_mt)?;
    }

    // ---- node methods (children / getchild / getparent / getscript / find) ----------
    let methods = lua.create_table()?;
    {
        let scene = shared.scene.clone();
        methods.set(
            "children",
            lua.create_function(move |lua, this: Table| {
                let e: u32 = this.raw_get("__id")?;
                let kids = scene.borrow().children.get(&e).cloned().unwrap_or_default();
                let arr = lua.create_table()?;
                for (i, c) in kids.iter().enumerate() {
                    arr.set(i + 1, new_node_handle(lua, *c)?)?;
                }
                Ok(arr)
            })?,
        )?;
    }
    {
        let scene = shared.scene.clone();
        let f = lua.create_function(move |lua, (this, name): (Table, String)| {
            let e: u32 = this.raw_get("__id")?;
            let found = {
                let s = scene.borrow();
                s.children
                    .get(&e)
                    .into_iter()
                    .flatten()
                    .copied()
                    .find(|c| s.names.get(c).map(|n| n == &name).unwrap_or(false))
            };
            Ok(match found {
                Some(c) => Value::Table(new_node_handle(lua, c)?),
                None => Value::Nil,
            })
        })?;
        methods.set("child", f.clone())?;
        methods.set("getchild", f)?;
    }
    {
        let scene = shared.scene.clone();
        methods.set(
            "getparent",
            lua.create_function(move |lua, this: Table| {
                let e: u32 = this.raw_get("__id")?;
                let p = scene.borrow().parent.get(&e).copied();
                Ok(match p {
                    Some(p) => Value::Table(new_node_handle(lua, p)?),
                    None => Value::Nil,
                })
            })?,
        )?;
    }
    {
        let scene = shared.scene.clone();
        let f = lua.create_function(move |lua, (this, name): (Table, String)| {
            let e: u32 = this.raw_get("__id")?;
            let has = scene
                .borrow()
                .scripts
                .get(&e)
                .map(|v| v.iter().any(|k| k == &name))
                .unwrap_or(false);
            Ok(if has {
                Value::Table(new_script_handle(lua, e, &name)?)
            } else {
                Value::Nil
            })
        })?;
        methods.set("script", f.clone())?;
        methods.set("getscript", f)?;
    }
    // node:getcomponent("PointLight" | "RigidBody") → a component handle whose numeric
    // fields you can read + assign (writes flush to the ECS after the frame), or nil if the
    // node has no such component.
    {
        let scene = shared.scene.clone();
        let f = lua.create_function(move |lua, (this, name): (Table, String)| {
            let e: u32 = this.raw_get("__id")?;
            let has =
                scene.borrow().components.get(&e).map(|c| c.contains_key(&name)).unwrap_or(false);
            Ok(if has {
                Value::Table(new_component_handle(lua, e, &name)?)
            } else {
                Value::Nil
            })
        })?;
        methods.set("component", f.clone())?;
        methods.set("getcomponent", f)?;
    }
    {
        let scene = shared.scene.clone();
        methods.set(
            "find",
            lua.create_function(move |lua, (this, name): (Table, String)| {
                let e: u32 = this.raw_get("__id")?;
                let found = {
                    let s = scene.borrow();
                    let mut stack: Vec<u32> =
                        s.children.get(&e).cloned().unwrap_or_default();
                    let mut hit = None;
                    while let Some(c) = stack.pop() {
                        if s.names.get(&c).map(|n| n == &name).unwrap_or(false) {
                            hit = Some(c);
                            break;
                        }
                        if let Some(cc) = s.children.get(&c) {
                            stack.extend(cc.iter().copied());
                        }
                    }
                    hit
                };
                Ok(match found {
                    Some(c) => Value::Table(new_node_handle(lua, c)?),
                    None => Value::Nil,
                })
            })?,
        )?;
    }
    lua.set_named_registry_value("floptle_node_methods", methods)?;

    // ---- script metatable -----------------------------------------------------------
    let script_mt = lua.create_table()?;
    {
        let envs = shared.envs.clone();
        let idx = lua.create_function(move |lua, (this, key): (Table, String)| {
            let e: u32 = this.raw_get("__id")?;
            let name: String = this.raw_get("__script")?;
            match key.as_str() {
                "node" => return Ok(Value::Table(new_node_handle(lua, e)?)),
                "kind" | "name" => return Ok(Value::String(lua.create_string(&name)?)),
                "valid" => {
                    return Ok(Value::Boolean(envs.borrow().contains_key(&(e, name.clone()))));
                }
                _ => {}
            }
            let env = envs.borrow().get(&(e, name)).cloned();
            match env {
                Some(env) => env.get::<Value>(key),
                None => Ok(Value::Nil),
            }
        })?;
        script_mt.set("__index", idx)?;
    }
    {
        let envs = shared.envs.clone();
        let newidx = lua.create_function(move |_, (this, key, val): (Table, String, Value)| {
            let e: u32 = this.raw_get("__id")?;
            let name: String = this.raw_get("__script")?;
            let env = envs.borrow().get(&(e, name)).cloned();
            if let Some(env) = env {
                env.set(key, val)?;
            }
            Ok(())
        })?;
        script_mt.set("__newindex", newidx)?;
    }
    lua.set_named_registry_value("floptle_script_mt", script_mt)?;

    // ---- globals: find / findAll / findScript ---------------------------------------
    {
        let scene = shared.scene.clone();
        lua.globals().set(
            "find",
            lua.create_function(move |lua, name: String| {
                let found = {
                    let s = scene.borrow();
                    s.order.iter().copied().find(|e| s.names.get(e).map(|n| n == &name).unwrap_or(false))
                };
                Ok(match found {
                    Some(e) => Value::Table(new_node_handle(lua, e)?),
                    None => Value::Nil,
                })
            })?,
        )?;
    }
    {
        let scene = shared.scene.clone();
        lua.globals().set(
            "findAll",
            lua.create_function(move |lua, name: String| {
                let ids: Vec<u32> = {
                    let s = scene.borrow();
                    s.order.iter().copied().filter(|e| s.names.get(e).map(|n| n == &name).unwrap_or(false)).collect()
                };
                let arr = lua.create_table()?;
                for (i, e) in ids.iter().enumerate() {
                    arr.set(i + 1, new_node_handle(lua, *e)?)?;
                }
                Ok(arr)
            })?,
        )?;
    }
    {
        let scene = shared.scene.clone();
        let f = lua.create_function(move |lua, kind: String| {
            let found = {
                let s = scene.borrow();
                s.order
                    .iter()
                    .copied()
                    .find(|e| s.scripts.get(e).map(|v| v.iter().any(|k| k == &kind)).unwrap_or(false))
                    .map(|e| (e, kind.clone()))
            };
            Ok(match found {
                Some((e, k)) => Value::Table(new_script_handle(lua, e, &k)?),
                None => Value::Nil,
            })
        })?;
        lua.globals().set("findScript", f.clone())?;
        lua.globals().set("findScriptInScene", f)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_core::transform::Transform;
    use floptle_core::{Scripts, World};
    use std::io::Write;

    fn write_script(dir: &Path, name: &str, body: &str) {
        let mut f = std::fs::File::create(dir.join(format!("{name}.lua"))).unwrap();
        f.write_all(body.as_bytes()).unwrap();
    }

    #[test]
    fn rotate_script_drives_yaw() {
        let dir = std::env::temp_dir().join("floptle_script_test_rotate");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "rotate",
            "defaults = { speed = 90 }\nfunction update(node, dt)\n  node.yaw = node.yaw + math.rad(params.speed) * dt\nend\n",
        );

        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Scripts(vec![floptle_core::ScriptInst {
            kind: "rotate".into(),
            enabled: true,
            params: vec![("speed".into(), 90.0)],
        }]));

        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0, 1.0); // 90 deg/s for 1s -> ~pi/2 yaw
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let tr = world.get::<Transform>(e).unwrap();
        let (yaw, _, _) = tr.rotation.to_euler(EulerRot::YXZ);
        assert!((yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-3, "yaw was {yaw}");
    }

    #[test]
    fn params_seeded_from_defaults_without_overrides() {
        // A script with `defaults` but NO per-instance overrides must still see params.X
        // (the bug: params was empty, so params.speed read nil).
        let dir = std::env::temp_dir().join("floptle_script_test_params_default");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "spin",
            "defaults = { speed = 90 }\nfunction update(node, dt)\n  node.yaw = node.yaw + math.rad(params.speed) * dt\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst { kind: "spin".into(), enabled: true, params: vec![] }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0, 1.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let (yaw, _, _) = world.get::<Transform>(e).unwrap().rotation.to_euler(EulerRot::YXZ);
        assert!((yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-3, "params.speed default not applied; yaw {yaw}");
    }

    #[test]
    fn script_can_raycast() {
        let dir = std::env::temp_dir().join("floptle_script_test_raycast");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "caster",
            "function update(node, dt)\n  local h = raycast(0, 5, 0, 0, -1, 0, 20)\n  if h then node.y = h.y end\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst { kind: "caster".into(), enabled: true, params: vec![] }]),
        );
        let mut host = ScriptHost::new();
        host.set_colliders(vec![Box::new(floptle_physics::Plane::ground(0.0))]);
        host.run(&mut world, &dir, 0.1, 0.1);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let _ = host.take_colliders();
        let y = world.get::<Transform>(e).unwrap().translation.y;
        assert!(y.abs() < 0.1, "raycast should have set y to the ground (≈0), got {y}");
    }

    #[test]
    fn preprocess_rewrites_compound_ops() {
        assert_eq!(preprocess("x += y"), "x = x + (y)");
        assert_eq!(preprocess("tbl.k *= 2"), "tbl.k = tbl.k * (2)");
        assert_eq!(preprocess("a[i] -= f()"), "a[i] = a[i] - (f())");
        assert_eq!(preprocess("s ..= 'z'"), "s = s .. ('z')");
        assert_eq!(preprocess("p %= 3"), "p = p % (3)");
        assert_eq!(preprocess("q ^= 2"), "q = q ^ (2)");
        assert_eq!(preprocess("n /= 2"), "n = n / (2)");
        // Precedence: the whole RHS is parenthesized.
        assert_eq!(preprocess("x *= a + b"), "x = x * (a + b)");
        // Nested index lvalue, balanced brackets.
        assert_eq!(preprocess("a[b[i]] += 1"), "a[b[i]] = a[b[i]] + (1)");
        // Inline block (lvalue back-scan stops at the keyword boundary).
        assert_eq!(preprocess("if c then x += 1 end"), "if c then x = x + (1) end");
    }

    #[test]
    fn preprocess_ignores_strings_and_comments() {
        assert_eq!(preprocess("s = 'x += y'"), "s = 'x += y'");
        assert_eq!(preprocess("-- x += y"), "-- x += y");
        assert_eq!(preprocess("t = [[ a += b ]]"), "t = [[ a += b ]]");
        assert_eq!(preprocess("t = [==[ a += b ]==]"), "t = [==[ a += b ]==]");
        assert_eq!(preprocess("if a == b then end"), "if a == b then end");
        assert_eq!(preprocess("c = a .. b"), "c = a .. b"); // concat untouched
        assert_eq!(preprocess("x = -y"), "x = -y"); // unary minus untouched
    }

    #[test]
    fn preprocess_preserves_line_count() {
        let src = "x += 1\ny -= 2\n-- z += 3\n";
        assert_eq!(preprocess(src).matches('\n').count(), src.matches('\n').count());
    }

    #[test]
    fn preprocess_closes_rhs_at_comments_and_statements() {
        // Trailing comment must not be swallowed into the RHS parentheses.
        assert_eq!(preprocess("x += 1 -- note"), "x = x + (1) -- note");
        assert_eq!(preprocess("s ..= 'z' -- c"), "s = s .. ('z') -- c");
        // A call/parenthesized receiver lvalue is captured whole.
        assert_eq!(preprocess("f().x += 1"), "f().x = f().x + (1)");
        assert_eq!(preprocess("(a).b -= 2"), "(a).b = (a).b - (2)");
        // A statement-introducing keyword on the same line terminates the RHS.
        assert_eq!(
            preprocess("function f() x += 1 return x end"),
            "function f() x = x + (1) return x end"
        );
        assert_eq!(preprocess("while c do n += 1 end"), "while c do n = n + (1) end");
    }

    #[test]
    fn compound_assignment_runs_end_to_end() {
        let dir = std::env::temp_dir().join("floptle_script_test_compound");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "spin",
            "defaults = { speed = 90 }\nfunction update(node, dt)\n  node.yaw += math.rad(params.speed) * dt\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Scripts(vec![floptle_core::ScriptInst {
            kind: "spin".into(),
            enabled: true,
            params: vec![("speed".into(), 90.0)],
        }]));
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0, 1.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let tr = world.get::<Transform>(e).unwrap();
        let (yaw, _, _) = tr.rotation.to_euler(EulerRot::YXZ);
        assert!((yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-3, "yaw was {yaw}");
    }

    #[test]
    fn script_reads_grounded_and_writes_velocity() {
        // The physics API: a script reads node.grounded + sets node.vx; the engine
        // reads that velocity back via take_body_changes.
        let dir = std::env::temp_dir().join("floptle_script_test_physapi");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "drive",
            "function update(node, dt)\n  if node.grounded then node.vx = 5.0 end\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Scripts(vec![floptle_core::ScriptInst {
            kind: "drive".into(),
            enabled: true,
            params: Vec::new(),
        }]));
        let mut host = ScriptHost::new();
        let mut bodies = HashMap::new();
        bodies.insert(
            e.index(),
            BodyState { vel: [0.0; 3], up: [0.0, 1.0, 0.0], grounded: true, height: 2.0 },
        );
        host.set_bodies(bodies);
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let changes = host.take_body_changes();
        assert_eq!(changes.get(&e.index()).copied().unwrap()[0], 5.0);
    }

    #[test]
    fn defaults_are_read() {
        let dir = std::env::temp_dir().join("floptle_script_test_defaults");
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "pulsate", "defaults = { amplitude = 0.3, speed = 2.0, base = 1.0 }\n");
        let host = ScriptHost::new();
        let d = host.script_defaults(&dir.join("pulsate.lua"));
        assert_eq!(d.len(), 3);
        assert!(d.iter().any(|(k, v)| k == "amplitude" && (*v - 0.3).abs() < 1e-6));
    }

    fn world_with_script(kind: &str) -> (World, Entity) {
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Scripts(vec![floptle_core::ScriptInst {
            kind: kind.into(),
            enabled: true,
            params: vec![],
        }]));
        (world, e)
    }

    #[test]
    fn captures_print_and_log() {
        let dir = std::env::temp_dir().join("floptle_script_test_logs");
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "talky", "function update(node, dt)\n  log('tick')\n  print('p', 2, true)\nend\n");
        let (mut world, _e) = world_with_script("talky");
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.1, 0.1);
        let logs = host.drain_logs();
        assert!(logs.iter().any(|l| l.msg == "tick" && l.level == LogLevel::Debug), "logs: {logs:?}");
        assert!(logs.iter().any(|l| l.msg == "p\t2\ttrue"), "logs: {logs:?}");
        // logs carry the originating script name for jump-to-source.
        assert!(logs.iter().any(|l| l.source.as_ref().is_some_and(|(n, _)| n == "talky")), "no source: {logs:?}");
        assert!(host.drain_logs().is_empty(), "logs should be drained");
    }

    #[test]
    fn captures_errors_in_console_feed() {
        let dir = std::env::temp_dir().join("floptle_script_test_err");
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "broken", "function update(node, dt)\n  this_is_not_defined()\nend\n");
        let (mut world, _e) = world_with_script("broken");
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.1, 0.1);
        assert!(!host.errors().is_empty(), "should report an error");
        let logs = host.drain_logs();
        assert!(logs.iter().any(|l| l.level == LogLevel::Error), "expected an error log: {logs:?}");
        assert!(logs.iter().any(|l| l.source.as_ref().is_some_and(|(n, _)| n == "broken")), "error lacks source: {logs:?}");
    }

    #[test]
    fn input_api_drives_a_script() {
        let dir = std::env::temp_dir().join("floptle_script_test_input");
        let _ = std::fs::create_dir_all(&dir);
        // Move +z while "w" is held; jump (+y) on the click edge.
        write_script(
            &dir,
            "mover",
            "function update(node, dt)\n  if input.key('w') then node.z = node.z + 1.0 end\n  if input.clicked(0) then node.y = node.y + 5.0 end\nend\n",
        );
        let (mut world, e) = world_with_script("mover");
        let mut host = ScriptHost::new();

        // No input → no movement.
        host.run(&mut world, &dir, 0.1, 0.1);
        assert_eq!(world.get::<Transform>(e).unwrap().translation.z, 0.0);

        // Hold "w" + click → moves +z and jumps +y.
        let mut snap = InputSnapshot::default();
        snap.keys_down.insert("w".into());
        snap.buttons_pressed[0] = true;
        host.set_input(snap);
        host.run(&mut world, &dir, 0.1, 0.1);
        let t = world.get::<Transform>(e).unwrap();
        assert!(t.translation.z >= 1.0, "w should move +z, z={}", t.translation.z);
        assert!(t.translation.y >= 5.0, "click should jump +y, y={}", t.translation.y);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    }

    #[test]
    fn input_released_edge() {
        let dir = std::env::temp_dir().join("floptle_script_test_released");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "rel",
            "function update(node, dt)\n  if input.released('e') then node.x = node.x + 1 end\nend\n",
        );
        let (mut world, e) = world_with_script("rel");
        let mut host = ScriptHost::new();
        // Release edge → +1.
        let mut snap = InputSnapshot::default();
        snap.keys_released.insert("e".into());
        host.set_input(snap);
        host.run(&mut world, &dir, 0.1, 0.0);
        assert!((world.get::<Transform>(e).unwrap().translation.x - 1.0).abs() < 1e-6);
        // No release → unchanged.
        host.set_input(InputSnapshot::default());
        host.run(&mut world, &dir, 0.1, 0.0);
        assert!((world.get::<Transform>(e).unwrap().translation.x - 1.0).abs() < 1e-6);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    }

    #[test]
    fn node_hierarchy_traversal() {
        let dir = std::env::temp_dir().join("floptle_script_test_hier");
        let _ = std::fs::create_dir_all(&dir);
        // A child reads its parent's x (+1) and finds a sibling by name.
        write_script(
            &dir,
            "reader",
            "function update(node, dt)\n  local p = node.parent\n  if p then node.x = p.x + 1 end\nend\n",
        );
        let mut world = World::default();
        let parent = world.spawn();
        world.insert(
            parent,
            Transform::from_translation(floptle_core::math::DVec3::new(10.0, 0.0, 0.0)),
        );
        world.insert(parent, floptle_core::Name("Parent".into()));
        let child = world.spawn();
        world.insert(child, Transform::IDENTITY);
        world.insert(child, floptle_core::Parent(parent));
        world.insert(child, floptle_core::Name("Child".into()));
        world.insert(
            child,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "reader".into(),
                enabled: true,
                params: vec![],
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.016, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        // child.x = parent.x + 1 = 11 (local transforms, like the `node` argument).
        assert!(
            (world.get::<Transform>(child).unwrap().translation.x - 11.0).abs() < 1e-6,
            "child.x = {}",
            world.get::<Transform>(child).unwrap().translation.x
        );
    }

    #[test]
    fn cross_script_reference_method_and_state() {
        let dir = std::env::temp_dir().join("floptle_script_test_xref");
        let _ = std::fs::create_dir_all(&dir);
        // A manager holds state + a method; the method moves its own node via `node`.
        write_script(
            &dir,
            "manager",
            "score = 0\nfunction addScore(n)\n  score = score + n\n  node.x = score\nend\nfunction update(node, dt) end\n",
        );
        // A ticker finds the manager anywhere in the scene and calls its method.
        write_script(
            &dir,
            "ticker",
            "function update(node, dt)\n  local m = findScript('manager')\n  if m then m.addScore(5) end\nend\n",
        );
        let mut world = World::default();
        let mgr = world.spawn();
        world.insert(mgr, Transform::IDENTITY);
        world.insert(mgr, floptle_core::Name("Manager".into()));
        world.insert(
            mgr,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "manager".into(),
                enabled: true,
                params: vec![],
            }]),
        );
        let t = world.spawn();
        world.insert(t, Transform::IDENTITY);
        world.insert(
            t,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "ticker".into(),
                enabled: true,
                params: vec![],
            }]),
        );
        let mut host = ScriptHost::new();
        for _ in 0..3 {
            host.run(&mut world, &dir, 0.016, 0.0);
        }
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        // 3 frames × +5 = 15; the manager moved itself to x = score via its node handle.
        assert!(
            (world.get::<Transform>(mgr).unwrap().translation.x - 15.0).abs() < 1e-6,
            "manager.x = {}",
            world.get::<Transform>(mgr).unwrap().translation.x
        );
    }

    #[test]
    fn script_reads_and_swaps_mesh_model() {
        // node.model reflects the current Mesh asset; assigning it swaps the model
        // (applied to the ECS in run + reported via take_model_changes for re-import).
        let dir = std::env::temp_dir().join("floptle_script_test_model");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "swap",
            "function update(node, dt)\n  if node.model == \"assets/models/old.glb\" then node.model = \"assets/models/new.glb\" end\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Matter::Mesh { asset_path: "assets/models/old.glb".into() });
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst { kind: "swap".into(), enabled: true, params: vec![] }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        match world.get::<Matter>(e).unwrap() {
            Matter::Mesh { asset_path } => assert_eq!(asset_path, "assets/models/new.glb"),
            other => panic!("expected mesh, got {other:?}"),
        }
        let changes = host.take_model_changes();
        assert_eq!(changes.get(&e.index()).map(|s| s.as_str()), Some("assets/models/new.glb"));
    }

    #[test]
    fn script_applies_material_preset() {
        // node.material = "<name>" resolves against the lent presets and inserts a Material.
        let dir = std::env::temp_dir().join("floptle_script_test_material");
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "paint", "function update(node, dt)\n  node.material = \"Gold\"\nend\n");
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Matter::Mesh { asset_path: "m.glb".into() });
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst { kind: "paint".into(), enabled: true, params: vec![] }]),
        );
        let mut host = ScriptHost::new();
        let mut mats = HashMap::new();
        mats.insert("Gold".to_string(), Material::tinted([1.0, 0.84, 0.0]));
        host.set_materials(mats);
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        let mat = world.get::<Material>(e).expect("material applied");
        assert_eq!(mat.color, [1.0, 0.84, 0.0]);
    }

    #[test]
    fn script_reads_and_writes_a_component_field() {
        // node:getcomponent("PointLight") reads the light's live fields, and assigning one
        // flushes back to the ECS the same frame.
        let dir = std::env::temp_dir().join("floptle_script_test_component");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "oscillate",
            "function update(node, dt)\n  local l = node:getcomponent(\"PointLight\")\n  if l then l.intensity = l.intensity + 1.0 end\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Matter::PointLight { color: [1.0, 1.0, 1.0], intensity: 2.0, range: 10.0 });
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst { kind: "oscillate".into(), enabled: true, params: vec![] }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        match world.get::<Matter>(e).unwrap() {
            Matter::PointLight { intensity, .. } => {
                assert!((*intensity - 3.0).abs() < 1e-4, "intensity became {intensity}, expected 3.0")
            }
            other => panic!("expected point light, got {other:?}"),
        }
    }

    #[test]
    fn script_toggles_visibility() {
        // node.visible reads true by default; assigning false attaches Visible(false).
        let dir = std::env::temp_dir().join("floptle_script_test_visible");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "hide",
            "function update(node, dt)\n  if node.visible then node.visible = false end\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Matter::Mesh { asset_path: "m.glb".into() });
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst { kind: "hide".into(), enabled: true, params: vec![] }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        assert_eq!(world.get::<Visible>(e).copied(), Some(Visible(false)));
    }

    #[test]
    fn assets_api_resolves_under_project_root() {
        // assets.getFile returns the path for an existing file (nil for a missing one);
        // assets.getContents lists a directory. Encode the three results into node.x.
        let root = std::env::temp_dir().join("floptle_script_test_assets_root");
        let models = root.join("models");
        let _ = std::fs::create_dir_all(&models);
        let _ = std::fs::write(models.join("armor.glb"), b"x");
        let dir = std::env::temp_dir().join("floptle_script_test_assets_scripts");
        let _ = std::fs::create_dir_all(&dir);
        write_script(
            &dir,
            "probe",
            "function update(node, dt)\n  local f = assets.getFile(\"models/armor.glb\")\n  local missing = assets.getFile(\"models/nope.glb\")\n  local c = assets.getContents(\"models\")\n  node.x = (f ~= nil and 1 or 0) + (missing == nil and 10 or 0) + (#c == 1 and 100 or 0)\nend\n",
        );
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst { kind: "probe".into(), enabled: true, params: vec![] }]),
        );
        let mut host = ScriptHost::new();
        host.set_project_root(root);
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 111.0);
    }
}
