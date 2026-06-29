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
//! function on_start(node) end            -- once, when play begins (optional)
//!
//! function on_update(node, dt)           -- every frame while playing
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

use std::collections::HashMap;
use std::path::Path;
use std::time::SystemTime;

use floptle_core::math::{DVec3, EulerRot, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_core::{Entity, Scripts, World};
use mlua::{Function, Lua, RegistryKey, Table, Value};

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
}

impl Default for ScriptHost {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptHost {
    pub fn new() -> Self {
        let lua = Lua::new();
        // Engine-branded logger (the Lua stdlib `print` also works).
        if let Ok(log) = lua.create_function(|_, msg: String| {
            eprintln!("[lua] {msg}");
            Ok(())
        }) {
            let _ = lua.globals().set("log", log);
        }
        Self { lua, sources: HashMap::new(), instances: HashMap::new(), errors: Vec::new() }
    }

    /// Errors raised by the most recent [`run`](Self::run) (one per failing script).
    pub fn errors(&self) -> &[String] {
        &self.errors
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
            self.errors.push(format!("{name}: script not found ({})", path.display()));
            return;
        };

        let key = (e.index(), name.to_string());
        // (Re)build the environment if missing or out of date with the source.
        let needs_build = self.instances.get(&key).is_none_or(|i| i.generation != generation);
        if needs_build {
            // Don't recompile a known-broken generation every frame; re-emit it.
            if let Some(err) = self.sources.get(name).and_then(|s| s.error.clone()) {
                self.errors.push(err);
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
        if first {
            if let Value::Function(f) = env.get::<Value>("on_start")? {
                f.call::<()>(node.clone())?;
            }
        }
        if let Value::Function(f) = env.get::<Value>("on_update")? {
            let f: Function = f;
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
        self.errors.push(msg);
    }
}

/// Build a fresh sandbox environment for a script: a table whose metatable falls
/// through to the real globals (so `math`, `string`, `log`, … are in scope) while
/// the script's own assignments stay local. Running the chunk defines its
/// functions (`on_start`, `on_update`) in that table.
fn build_env(lua: &Lua, src: &str, name: &str) -> mlua::Result<Table> {
    let env = lua.create_table()?;
    let mt = lua.create_table()?;
    mt.set("__index", lua.globals())?;
    env.set_metatable(Some(mt));
    lua.load(src).set_name(name).set_environment(env.clone()).exec()?;
    Ok(env)
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
            "defaults = { speed = 90 }\nfunction on_update(node, dt)\n  node.yaw = node.yaw + math.rad(params.speed) * dt\nend\n",
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
    fn defaults_are_read() {
        let dir = std::env::temp_dir().join("floptle_script_test_defaults");
        let _ = std::fs::create_dir_all(&dir);
        write_script(&dir, "pulsate", "defaults = { amplitude = 0.3, speed = 2.0, base = 1.0 }\n");
        let host = ScriptHost::new();
        let d = host.script_defaults(&dir.join("pulsate.lua"));
        assert_eq!(d.len(), 3);
        assert!(d.iter().any(|(k, v)| k == "amplitude" && (*v - 0.3).abs() < 1e-6));
    }
}
