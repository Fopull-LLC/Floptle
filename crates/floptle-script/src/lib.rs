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
use std::path::Path;
use std::rc::Rc;
use std::time::SystemTime;

use floptle_core::math::{DVec3, EulerRot, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_core::{Entity, Scripts, World};
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
            let _ = lua.globals().set("input", t);
        }
        Self {
            lua,
            sources: HashMap::new(),
            instances: HashMap::new(),
            errors: Vec::new(),
            logs,
            input,
            bodies: Rc::new(RefCell::new(HashMap::new())),
            body_changes: Rc::new(RefCell::new(HashMap::new())),
            body_height_changes: Rc::new(RefCell::new(HashMap::new())),
        }
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

        // Snapshot (entity, scripts) so we can mutate Transforms while iterating.
        let work: Vec<(Entity, Scripts)> =
            world.query::<Scripts>().map(|(e, s)| (e, s.clone())).collect();
        for (e, scripts) in &work {
            let Some(mut tr) = world.get::<Transform>(*e).copied() else { continue };
            let mut ran = false;
            for inst in &scripts.0 {
                if inst.enabled {
                    self.run_one(*e, &inst.kind, &inst.params, scripts_dir, &mut tr, dt, time);
                    ran = true;
                }
            }
            if ran {
                if let Some(slot) = world.get_mut::<Transform>(*e) {
                    *slot = tr;
                }
            }
        }

        // Drop environments whose (node, script) no longer exists.
        let stale: Vec<(u32, String)> =
            self.instances.iter().filter(|(_, i)| !i.seen).map(|(k, _)| k.clone()).collect();
        for k in stale {
            if let Some(inst) = self.instances.remove(&k) {
                let _ = self.lua.remove_registry_value(inst.env);
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

    fn run_one(
        &mut self,
        e: Entity,
        name: &str,
        params: &[(String, f32)],
        scripts_dir: &Path,
        tr: &mut Transform,
        dt: f32,
        time: f32,
    ) {
        let path = scripts_dir.join(format!("{name}.lua"));
        let Some(generation) = self.ensure_source(name, &path) else {
            self.record_error(name, format!("{name}: script not found ({})", path.display()));
            return;
        };

        let key = (e.index(), name.to_string());
        // (Re)build the environment if missing or out of date with the source.
        let needs_build = self.instances.get(&key).is_none_or(|i| i.generation != generation);
        if needs_build {
            // Don't recompile a known-broken generation every frame; re-emit it.
            if let Some(err) = self.sources.get(name).and_then(|s| s.error.clone()) {
                self.record_error(name, err);
                return;
            }
            let src = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(err) => {
                    self.fail(name, format!("{name}: {err}"));
                    return;
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
                            return;
                        }
                    }
                }
                Err(err) => {
                    self.fail(name, format!("{name}: {err}"));
                    return;
                }
            }
        }

        // Fetch the (now current) environment and drive the lifecycle.
        let Some(inst) = self.instances.get_mut(&key) else { return };
        inst.seen = true;
        let first = !inst.started;
        inst.started = true;
        let Ok(env) = self.lua.registry_value::<Table>(&inst.env) else { return };

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

        let node = node_table(&self.lua, tr, body)?;
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

fn node_table(lua: &Lua, tr: &Transform, body: Option<BodyState>) -> mlua::Result<Table> {
    let (yaw, pitch, roll) = tr.rotation.to_euler(EulerRot::YXZ);
    let t = lua.create_table()?;
    t.set("x", tr.translation.x)?;
    t.set("y", tr.translation.y)?;
    t.set("z", tr.translation.z)?;
    t.set("scale_x", tr.scale.x as f64)?;
    t.set("scale_y", tr.scale.y as f64)?;
    t.set("scale_z", tr.scale.z as f64)?;
    t.set("scale", tr.scale.x as f64)?; // uniform-scale shortcut
    t.set("yaw", yaw as f64)?;
    t.set("pitch", pitch as f64)?;
    t.set("roll", roll as f64)?;
    // Physics body fields (present only on rigidbody nodes): read grounded, read/write
    // the velocity. The engine reads vx/vy/vz back after `update` and applies them.
    if let Some(b) = body {
        t.set("vx", b.vel[0] as f64)?;
        t.set("vy", b.vel[1] as f64)?;
        t.set("vz", b.vel[2] as f64)?;
        t.set("up_x", b.up[0] as f64)?;
        t.set("up_y", b.up[1] as f64)?;
        t.set("up_z", b.up[2] as f64)?;
        t.set("grounded", b.grounded)?;
        t.set("height", b.height as f64)?; // write to crouch (capsule resizes, feet planted)
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
}
