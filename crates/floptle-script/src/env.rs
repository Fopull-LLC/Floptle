//! Per-instance sandbox plumbing: building a script's environment table, the
//! lifecycle function lookup, and the `node`/`params` tables synced to a
//! node's [`Transform`] before each call and read back after.

use std::path::Path;

use floptle_core::math::{DVec3, EulerRot, Quat, Vec3};
use floptle_core::transform::Transform;
use mlua::{Function, Lua, Table, Value};

use crate::preprocess::preprocess;
use crate::BodyState;

/// The pre-call `node` values, so we only write back fields the script changed
/// (avoids quat↔euler drift on untouched rotations, etc.).
pub(crate) struct NodePre {
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

/// Build a fresh sandbox environment for a script: a table whose metatable falls
/// through to the real globals (so `math`, `string`, `log`, … are in scope) while
/// the script's own assignments stay local. Running the chunk defines its
/// functions (`start`, `update`) in that table.
pub(crate) fn build_env(lua: &Lua, src: &str, name: &str) -> mlua::Result<Table> {
    let env = lua.create_table()?;
    let mt = lua.create_table()?;
    mt.set("__index", lua.globals())?;
    env.set_metatable(Some(mt));
    lua.load(&preprocess(src)).set_name(name).set_environment(env.clone()).exec()?;
    Ok(env)
}

/// The first of `names` that's a function in `env` (lets a hook have aliases).
pub(crate) fn lifecycle_fn(env: &Table, names: &[&str]) -> mlua::Result<Option<Function>> {
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
pub(crate) fn params_table(lua: &Lua, env: &Table, params: &[(String, f32)]) -> mlua::Result<Table> {
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

pub(crate) fn node_table(lua: &Lua, eid: u32, tr: &Transform, body: Option<BodyState>) -> mlua::Result<Table> {
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

pub(crate) fn node_pre(tr: &Transform) -> NodePre {
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
pub(crate) fn apply_node(t: &Table, tr: &mut Transform, pre: &NodePre) -> mlua::Result<()> {
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
pub(crate) fn new_node_handle(lua: &Lua, e: u32) -> mlua::Result<Table> {
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
pub(crate) fn new_component_handle(lua: &Lua, e: u32, comp: &str) -> mlua::Result<Table> {
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
pub(crate) fn new_script_handle(lua: &Lua, e: u32, name: &str) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.raw_set("__id", e)?;
    t.raw_set("__script", name)?;
    if let Ok(mt) = lua.named_registry_value::<Table>("floptle_script_mt") {
        t.set_metatable(Some(mt));
    }
    Ok(t)
}

pub(crate) fn as_num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => Some(*n),
        Value::Integer(n) => Some(*n as f64),
        _ => None,
    }
}

/// The preset name a `node.material = ...` ref resolves to: the file stem of a path
/// (`"assets/materials/Gold.ron"` → `"Gold"`) or the bare name as given (`"Gold"`).
pub(crate) fn material_key(refstr: &str) -> String {
    Path::new(refstr)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| refstr.to_string())
}
