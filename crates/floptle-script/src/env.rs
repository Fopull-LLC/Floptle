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
#[derive(Clone, Copy)]
pub(crate) struct NodePre {
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) z: f64,
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
    lua.load(preprocess(src)).set_name(name).set_environment(env.clone()).exec()?;
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

/// The sentinel `noderef()` returns — a `defaults` value of this string marks the
/// param as a NODE REFERENCE the Inspector wires to a scene node by name.
pub(crate) const NODEREF_SENTINEL: &str = "__floptle_noderef";
/// `scriptref("health")` → `__floptle_scriptref:health` — the param binds to
/// that SCRIPT on the wired node (the script sees a script handle directly).
pub(crate) const SCRIPTREF_PREFIX: &str = "__floptle_scriptref:";
/// `componentref("RigidBody")` → `__floptle_compref:RigidBody` — the param
/// binds to that COMPONENT on the wired node (a component handle directly).
pub(crate) const COMPREF_PREFIX: &str = "__floptle_compref:";

/// Parse a `defaults` sentinel value into the reference kind it declares.
pub(crate) fn parse_ref_sentinel(v: &str) -> Option<crate::RefKind> {
    if v == NODEREF_SENTINEL {
        Some(crate::RefKind::Node)
    } else if let Some(k) = v.strip_prefix(SCRIPTREF_PREFIX) {
        Some(crate::RefKind::Script(k.to_string()))
    } else {
        v.strip_prefix(COMPREF_PREFIX).map(|c| crate::RefKind::Component(c.to_string()))
    }
}

/// A reference param's binding for this tick, fully resolved + validated by the
/// host (the target exists and carries the declared script/component).
pub(crate) enum ResolvedRef {
    None,
    Node(u32),
    Script(u32, String),
    Component(u32, String),
}

/// Build the `params` table a script sees: its declared `defaults` as the base, with
/// any per-instance overrides (Inspector tweaks) layered on top. Seeding from `defaults`
/// is what makes `params.foo` resolve out of the box — without it, a script with no saved
/// overrides sees an empty `params` and every `params.foo` reads `nil`.
///
/// `refs` are the instance's reference params, resolved + validated by the host:
/// the script sees a node / script / component HANDLE — `params.hpBar.text = hp`,
/// `params.health.damage(5)`, `params.body.friction = 0` — zero `find()` calls.
/// Unwired or invalid targets read `nil` (so `if params.x then` guards work).
pub(crate) fn params_table(
    lua: &Lua,
    env: &Table,
    params: &[(String, f32)],
    refs: &[(String, ResolvedRef)],
    strs: &[(String, String)],
) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    if let Ok(defaults) = env.get::<Table>("defaults") {
        for (k, v) in defaults.pairs::<Value, Value>().flatten() {
            // Never leak a ref sentinel string: an unwired ref reads nil.
            if let Value::String(s) = &v
                && parse_ref_sentinel(&s.to_string_lossy()).is_some()
            {
                continue;
            }
            t.set(k, v)?;
        }
    }
    for (k, v) in params {
        t.set(k.as_str(), *v as f64)?;
    }
    // Stored STRING overrides land over the defaults, like the numbers above.
    for (k, v) in strs {
        t.set(k.as_str(), v.as_str())?;
    }
    for (k, r) in refs {
        match r {
            ResolvedRef::Node(id) => t.set(k.as_str(), new_node_handle(lua, *id)?)?,
            ResolvedRef::Script(id, kind) => {
                t.set(k.as_str(), new_script_handle(lua, *id, kind)?)?
            }
            ResolvedRef::Component(id, comp) => {
                t.set(k.as_str(), new_component_handle(lua, *id, comp)?)?
            }
            ResolvedRef::None => t.set(k.as_str(), Value::Nil)?,
        }
    }
    Ok(t)
}

/// Create the script's own-node table: `{__id}` under the node metatable, with the
/// transform stamped into it as direct fields.
///
/// The returned table is **kept for the life of the instance** and re-stamped before
/// every hook (see [`stamp_node_table`]) rather than rebuilt. That is what makes the
/// most natural thing a script author writes —
///
/// ```lua
/// function start(node) me = node end
/// ```
///
/// — keep working: `me` is the same table the engine goes on updating, so `me.x` on a
/// later hook is the CURRENT position. Building a fresh table per hook froze such a
/// handle at the spawn pose, silently, while everything using the passed `node` stayed
/// correct (floptle/0027).
pub(crate) fn node_table(lua: &Lua, eid: u32, tr: &Transform, body: Option<BodyState>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    // Tag with the entity + the node metatable so `node.parent`, `node:getscript(...)`,
    // `node:children()` etc. work. The transform fields below are direct table values, so
    // they're read/written normally (the metatable only supplies the missing keys).
    t.raw_set("__id", eid)?;
    if let Ok(mt) = lua.named_registry_value::<Table>("floptle_node_mt") {
        t.set_metatable(Some(mt));
    }
    stamp_node_table(&t, tr, body)?;
    Ok(t)
}

/// Write the node's current transform + body state into its own-node table, called
/// before every hook.
///
/// raw_set so these stay DIRECT table fields (not routed through the node metatable's
/// `__newindex`, which is for handles to other nodes) — the read-back path reads them
/// directly after the hook.
pub(crate) fn stamp_node_table(
    t: &Table,
    tr: &Transform,
    body: Option<BodyState>,
) -> mlua::Result<()> {
    let (yaw, pitch, roll) = tr.rotation.to_euler(EulerRot::YXZ);
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
    match body {
        Some(b) => {
            t.raw_set("vx", b.vel[0] as f64)?;
            t.raw_set("vy", b.vel[1] as f64)?;
            t.raw_set("vz", b.vel[2] as f64)?;
            t.raw_set("up_x", b.up[0] as f64)?;
            t.raw_set("up_y", b.up[1] as f64)?;
            t.raw_set("up_z", b.up[2] as f64)?;
            t.raw_set("grounded", b.grounded)?;
            t.raw_set("height", b.height as f64)?; // write to crouch (capsule resizes, feet planted)
            // The TICK pose channel (`docs/rollback-netcode-design.md` §3):
            // the body's own position, not the interpolated render pose that
            // `x`/`y`/`z` carry between ticks. Read it to build a hurtbox;
            // write it to move the body without going through the transform.
            t.raw_set("tickX", b.pos[0])?;
            t.raw_set("tickY", b.pos[1])?;
            t.raw_set("tickZ", b.pos[2])?;
        }
        // The node lost its RigidBody since the last hook — clear the stale body fields
        // rather than leaving the last tick's velocity readable on a table we now reuse.
        None => {
            for k in
                ["vx", "vy", "vz", "up_x", "up_y", "up_z", "grounded", "height", "tickX", "tickY", "tickZ"]
            {
                if t.raw_get::<Value>(k)? != Value::Nil {
                    t.raw_set(k, Value::Nil)?;
                }
            }
        }
    }
    Ok(())
}

/// The own-node table's values as the engine last left them. A difference against this
/// at the start of the next hook means something wrote to the table from OUTSIDE that
/// script's hook — a cross-script method call, a timer or an `on…` callback — which the
/// post-hook read-back never saw. [`drain_node_writes`] applies those before re-stamping.
#[derive(Clone, Copy)]
pub(crate) struct NodeStamp {
    pre: NodePre,
    vel: Option<[f32; 3]>,
    height: Option<f32>,
    tick_pos: Option<[f64; 3]>,
}

/// Snapshot the table's current values, to compare the next hook against.
pub(crate) fn node_stamp(t: &Table, tr: &Transform) -> NodeStamp {
    let mut pre = node_pre(tr);
    // Read from the TABLE, not the transform: after a hook these differ whenever the
    // script's write was queued rather than applied in place (a body teleport).
    if let Ok(v) = t.raw_get::<f64>("x") {
        pre.x = v;
    }
    if let Ok(v) = t.raw_get::<f64>("y") {
        pre.y = v;
    }
    if let Ok(v) = t.raw_get::<f64>("z") {
        pre.z = v;
    }
    for (slot, key) in [
        (&mut pre.sx, "scale_x"),
        (&mut pre.sy, "scale_y"),
        (&mut pre.sz, "scale_z"),
        (&mut pre.scale, "scale"),
        (&mut pre.yaw, "yaw"),
        (&mut pre.pitch, "pitch"),
        (&mut pre.roll, "roll"),
    ] {
        if let Ok(v) = t.raw_get::<f64>(key) {
            *slot = v;
        }
    }
    let vel = match (t.raw_get::<f64>("vx"), t.raw_get::<f64>("vy"), t.raw_get::<f64>("vz")) {
        (Ok(x), Ok(y), Ok(z)) => Some([x as f32, y as f32, z as f32]),
        _ => None,
    };
    let tick_pos =
        match (t.raw_get::<f64>("tickX"), t.raw_get::<f64>("tickY"), t.raw_get::<f64>("tickZ")) {
            (Ok(x), Ok(y), Ok(z)) => Some([x, y, z]),
            _ => None,
        };
    NodeStamp {
        pre,
        vel,
        height: t.raw_get::<f64>("height").ok().map(|h| h as f32),
        tick_pos,
    }
}

/// What a script wrote to its own-node table between hooks.
#[derive(Default)]
pub(crate) struct DrainedWrites {
    pub(crate) moved: bool,
    pub(crate) vel: Option<[f32; 3]>,
    pub(crate) height: Option<f32>,
    /// A write to `node.tickX/tickY/tickZ` (or `node.tickPos`) — a direct body
    /// teleport in the tick channel, bypassing the render transform entirely.
    pub(crate) tick_pos: Option<[f64; 3]>,
}

/// Apply writes made to the own-node table since [`node_stamp`] was taken, so a
/// cross-script `other:knockBack()` that sets `me.x` is not silently thrown away by the
/// next re-stamp. The transform half reuses the same only-what-changed comparison the
/// post-hook read-back uses; the caller queues the body half.
pub(crate) fn drain_node_writes(
    t: &Table,
    prev: &NodeStamp,
    tr: &mut Transform,
) -> mlua::Result<DrainedWrites> {
    let before = tr.translation;
    apply_node(t, tr, &prev.pre)?;
    let mut out = DrainedWrites { moved: tr.translation != before, ..Default::default() };
    if let Some(pv) = prev.vel
        && let (Ok(x), Ok(y), Ok(z)) =
            (t.raw_get::<f64>("vx"), t.raw_get::<f64>("vy"), t.raw_get::<f64>("vz"))
    {
        let now = [x as f32, y as f32, z as f32];
        if now != pv {
            out.vel = Some(now);
        }
    }
    if let Some(ph) = prev.height
        && let Ok(h) = t.raw_get::<f64>("height")
        && h as f32 != ph
    {
        out.height = Some(h as f32);
    }
    if let Some(pt) = prev.tick_pos
        && let (Ok(x), Ok(y), Ok(z)) =
            (t.raw_get::<f64>("tickX"), t.raw_get::<f64>("tickY"), t.raw_get::<f64>("tickZ"))
    {
        let now = [x, y, z];
        if now != pt {
            out.tick_pos = Some(now);
        }
    }
    Ok(out)
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
