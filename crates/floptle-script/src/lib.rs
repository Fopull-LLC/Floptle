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
        }
    }

    /// Set the player input for the frame's scripts (call before [`run`](Self::run)).
    pub fn set_input(&self, snapshot: InputSnapshot) {
        *self.input.borrow_mut() = snapshot;
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

        if let Err(err) = self.tick(&env, params, tr, dt, time, first) {
            self.fail(name, format!("{name}: {err}"));
        }
    }

    /// One lifecycle tick against an already-built environment.
    fn tick(
        &self,
        env: &Table,
        params: &[(String, f32)],
        tr: &mut Transform,
        dt: f32,
        time: f32,
        first: bool,
    ) -> mlua::Result<()> {
        env.set("params", params_table(&self.lua, params)?)?;
        env.set("time", time as f64)?;
        env.set("dt", dt as f64)?;

        let node = node_table(&self.lua, tr)?;
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
        if c.is_ascii_alphanumeric() || c == b'_' || c == b'.' {
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

        // A block-ending keyword also terminates a rewritten RHS (e.g. the `end` in
        // `if c then x += 1 end`). These are reserved words, so they can't be part of
        // the expression. Close the paren before copying the keyword.
        if pending_close && (c.is_ascii_alphabetic() || c == b'_') {
            let prev_ident = out.last().is_some_and(|&p| p.is_ascii_alphanumeric() || p == b'_');
            if !prev_ident {
                let mut k = i;
                while k < n && (b[k].is_ascii_alphanumeric() || b[k] == b'_') {
                    k += 1;
                }
                let word = std::str::from_utf8(&b[i..k]).unwrap_or("");
                if matches!(word, "end" | "else" | "elseif" | "then" | "do" | "until") {
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

fn params_table(lua: &Lua, params: &[(String, f32)]) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    for (k, v) in params {
        t.set(k.as_str(), *v as f64)?;
    }
    Ok(t)
}

fn node_table(lua: &Lua, tr: &Transform) -> mlua::Result<Table> {
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
