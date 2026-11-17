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
///
/// Raw first. A script's own `update` is a direct field of its env — running the
/// chunk put it there — and on Luau an mlua `get` against a table that has a
/// metatable takes the protected path and allocates whether or not `__index` is
/// ever consulted. The second loop keeps the old answer for a hook that is not
/// the script's own: `__index` falls through to the real globals.
pub(crate) fn lifecycle_fn(env: &Table, names: &[&str]) -> mlua::Result<Option<Function>> {
    for n in names {
        if let Value::Function(f) = env.raw_get::<Value>(*n)? {
            return Ok(Some(f));
        }
    }
    for n in names {
        if let Value::Function(f) = env.get::<Value>(*n)? {
            return Ok(Some(f));
        }
    }
    Ok(None)
}

/// The sentinel `noderef()` returns — a `defaults` value of this string marks the
/// param as a node reference the Inspector wires to a scene node by name.
pub(crate) const NODEREF_SENTINEL: &str = "__floptle_noderef";
/// `scriptref("health")` → `__floptle_scriptref:health` — the param binds to
/// that script on the wired node (the script sees a script handle directly).
pub(crate) const SCRIPTREF_PREFIX: &str = "__floptle_scriptref:";
/// `componentref("RigidBody")` → `__floptle_compref:RigidBody` — the param
/// binds to that component on the wired node (a component handle directly).
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
/// the script sees a node / script / component handle — `params.hpBar.text = hp`,
/// `params.health.damage(5)`, `params.body.friction = 0` — zero `find()` calls.
/// Unwired or invalid targets read `nil` (so `if params.x then` guards work).
pub(crate) fn params_table(
    lua: &Lua,
    env: &Table,
    params: &[(String, f64)],
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
        // Stored params are floats, but a tunable the script declared as a
        // Boolean must read back as one — otherwise `if params.invert then`
        // would be true for a stored 0 (every number is truthy in Lua), which is
        // the opposite of what the Inspector's unticked checkbox says.
        let declared_bool = env
            .get::<Table>("defaults")
            .ok()
            .and_then(|d| d.get::<Value>(k.as_str()).ok())
            .is_some_and(|d| matches!(d, Value::Boolean(_)));
        if declared_bool {
            t.set(k.as_str(), *v != 0.0)?;
        } else {
            t.set(k.as_str(), *v)?;
        }
    }
    // Stored string overrides land over the defaults, like the numbers above.
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

// Armed tables, from the vendored Luau (`third_party/luau0-src`, see
// FLOPTLE-PATCHES.md). An armed table is an ordinary table to every script;
// the first write of any kind to it (a field, `rawset`, `setmetatable`,
// `table.*`, a raw set from the engine) quietly disarms it. So "still armed"
// means "nothing has written to it since it was armed", read with no Lua call.
unsafe extern "C" {
    fn lua_floptle_arm(p: *const std::ffi::c_void);
    fn lua_floptle_armed(p: *const std::ffi::c_void) -> std::ffi::c_int;
}

/// Arm `t`, so [`armed_at`] answers true until something writes to it. A
/// frozen table stays frozen.
pub(crate) fn arm(t: &Table) {
    // SAFETY: `t` is a live table, and its address is what the patch reads.
    unsafe { lua_floptle_arm(t.to_pointer()) }
}

/// Whether the table at `p` (from [`Table::to_pointer`]) is still armed.
///
/// # Safety
/// `p` must be the address of a table that is still alive: one the caller
/// holds a reference or registry key to. Luau's collector never moves an
/// object, so the address stays good for as long as the table lives.
pub(crate) unsafe fn armed_at(p: *const std::ffi::c_void) -> bool {
    // SAFETY: the caller's promise.
    unsafe { lua_floptle_armed(p) != 0 }
}

/// [`armed_at`] for a table in hand.
#[cfg(test)]
pub(crate) fn is_armed(t: &Table) -> bool {
    // SAFETY: `t` keeps the table alive for the call.
    unsafe { armed_at(t.to_pointer()) }
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
/// later hook is the current position. Building a fresh table per hook froze such a
/// handle at the spawn pose, silently, while everything using the passed `node` stayed
/// correct.
/// Queue a body teleport for the driver, and make it what every later read
/// sees.
///
/// The driver applies the queue between passes, and until then the body
/// bridge still holds the pose from before the write. A node table stamped in
/// that gap, or a `tickPos` read through another node's handle, answered with
/// where the body was: a teleport made through a method call read back as the
/// old position for the whole next tick.
pub(crate) fn queue_body_pos(
    queue: &std::cell::RefCell<std::collections::HashMap<u32, [f64; 3]>>,
    bodies: &std::cell::RefCell<std::collections::HashMap<u32, BodyState>>,
    e: u32,
    p: [f64; 3],
) {
    queue.borrow_mut().insert(e, p);
    if let Some(b) = bodies.borrow_mut().get_mut(&e) {
        b.pos = p;
    }
}

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
/// raw_set so these stay direct table fields (not routed through the node metatable's
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
            // The tick pose channel (`docs/multiplayer.md` §3):
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
/// at the start of the next hook means something wrote to the table from outside that
/// script's hook — a cross-script method call, a timer or an `on…` callback — which the
/// post-hook read-back never saw. [`drain_node_writes`] applies those before re-stamping.
#[derive(Clone, Copy)]
pub(crate) struct NodeStamp {
    pre: NodePre,
    vel: Option<[f32; 3]>,
    height: Option<f32>,
    tick_pos: Option<[f64; 3]>,
    /// The transform the table was last stamped from, when nothing has run
    /// on it since: the stamp was the last thing to touch it, and the node
    /// has no body. Then the table holds exactly what [`stamp_node_table`]
    /// writes for this transform, so a later stamp from the same transform
    /// onto a table nobody wrote changes nothing — see [`stamp_unchanged`].
    /// `None` after a hook, which may have written anything.
    clean: Option<Transform>,
}

/// An instance's kept `node` table: the registry key that keeps it alive,
/// what the engine last stamped into it, and its address, for asking the VM
/// whether anything has written to it since (see [`arm`]). The address is
/// only ever read while `key` is held, which is what makes reading it sound:
/// the registry keeps the table alive, and Luau never moves it.
pub(crate) struct KeptNode {
    pub(crate) key: mlua::RegistryKey,
    pub(crate) stamp: NodeStamp,
    ptr: usize,
}

impl KeptNode {
    pub(crate) fn new(lua: &Lua, t: &Table, stamp: NodeStamp) -> mlua::Result<Self> {
        Ok(Self { key: lua.create_registry_value(t)?, stamp, ptr: t.to_pointer() as usize })
    }

    /// Nothing has written to the table since the engine last armed it.
    pub(crate) fn armed(&self) -> bool {
        // SAFETY: `self.key` keeps the table at `ptr` alive.
        unsafe { armed_at(self.ptr as *const std::ffi::c_void) }
    }

    /// The table is exactly what the last stamp left for `tr`, known without
    /// reading it: the stamp was clean (see [`NodeStamp::clean`]) and from
    /// this same transform, the node still has no body, and nothing has
    /// written to the table since. The first tier of [`stamp_unchanged`],
    /// which the run loop also asks before anything else.
    pub(crate) fn at_rest(&self, tr: &Transform, has_body: bool) -> bool {
        !has_body && self.stamp.clean.is_some_and(|c| same_transform(&c, tr)) && armed_tier_on() && self.armed()
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only: answer "unchanged" from the table's own fields alone, as
    /// before tables were armed — the differential tests' and the ratio
    /// guard's other side.
    pub(crate) static CHUNK_ONLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Test-only: skip re-reading the table on every armed "unchanged"
    /// (see [`cross_check`]). Only the timing guards turn it off.
    pub(crate) static NO_CROSS_CHECK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[inline(always)]
fn armed_tier_on() -> bool {
    #[cfg(test)]
    if CHUNK_ONLY.with(|c| c.get()) {
        return false;
    }
    true
}

/// The stamp [`stamp_node_table`] leaves for `tr` and `body`, worked out
/// rather than read back: right after a stamp the table holds exactly the
/// values written from these, so reading seventeen of them back told the
/// engine what it had just said. Marked clean (see [`NodeStamp::clean`]).
pub(crate) fn stamp_of(tr: &Transform, body: Option<BodyState>) -> NodeStamp {
    NodeStamp {
        pre: node_pre(tr),
        // f32 → f64 → f32 is exact, so these equal what a read-back gives.
        vel: body.map(|b| b.vel),
        height: body.map(|b| b.height),
        tick_pos: body.map(|b| b.pos),
        clean: body.is_none().then_some(*tr),
    }
}

/// Bit for bit, so a stamp skipped because the transform "did not change"
/// can never be one that would have written a different number (a `-0.0`
/// for a `0.0`).
fn same_transform(a: &Transform, b: &Transform) -> bool {
    a.translation.to_array().map(f64::to_bits) == b.translation.to_array().map(f64::to_bits)
        && a.rotation.to_array().map(f32::to_bits) == b.rotation.to_array().map(f32::to_bits)
        && a.scale.to_array().map(f32::to_bits) == b.scale.to_array().map(f32::to_bits)
}

/// The Luau half of [`stamp_unchanged`]: a function answering whether the
/// table is, field for field, what the last stamp left. One call that reads
/// every field in the VM, rather than a round trip from Rust per field.
/// Called with the table's metatable set aside, so every read is the table's
/// own field — a field the script cleared reads nil here rather than reaching
/// the node metatable's `__index` — at a fraction of the cost of `rawget`,
/// which is a function call per field. A zero is compared by its sign too:
/// `-0` and `0` are equal to `==`, and a script that wrote `me.x = -me.x` at
/// zero has written a number the next stamp puts back.
pub(crate) fn stamp_unchanged_lua() -> String {
    let fields = [
        ("x", "x"),
        ("y", "y"),
        ("z", "z"),
        ("scale_x", "sx"),
        ("scale_y", "sy"),
        ("scale_z", "sz"),
        ("scale", "s"),
        ("yaw", "yaw"),
        ("pitch", "pitch"),
        ("roll", "roll"),
    ];
    let mut src = String::from("return function(t, id, x, y, z, sx, sy, sz, s, yaw, pitch, roll)\n  return t.__id == id");
    for (k, a) in fields {
        src += &format!("\n    and t.{k} == {a} and ({a} ~= 0 or 1 / t.{k} == 1 / {a})");
    }
    for k in ["vx", "vy", "vz", "up_x", "up_y", "up_z", "grounded", "height", "tickX", "tickY", "tickZ"] {
        src += &format!("\n    and t.{k} == nil");
    }
    src + "\nend\n"
}

/// Would draining and re-stamping node `eid`'s table `t` for `tr` (no body)
/// change nothing at all? True when the last stamp was the last thing to
/// touch the table, from this same transform, and every field it wrote (and
/// every body field it cleared) still reads as it left it — so there is no
/// write to drain and nothing a stamp would change. Any doubt answers false,
/// and the caller drains and stamps as it always did.
///
/// Two tiers. First the table's armed flag ([`KeptNode::at_rest`]): still
/// armed, nothing has written to it since the stamp, which answers without
/// reading it. Disarmed, something wrote — perhaps the same value, or a key
/// of its own — so the table is read field by field, by `same`
/// ([`stamp_unchanged_lua`]'s function), exactly as before tables were armed;
/// if that finds it unchanged it is armed again. Either way the answer is the
/// one the field-by-field read gives, which in tests is checked on every
/// armed answer.
pub(crate) fn stamp_unchanged(
    same: &Function,
    t: &Table,
    eid: u32,
    kept: &KeptNode,
    tr: &Transform,
    body: Option<BodyState>,
) -> bool {
    let stamp = &kept.stamp;
    if kept.at_rest(tr, body.is_some()) {
        #[cfg(test)]
        cross_check(same, t, eid, stamp);
        return true;
    }
    if body.is_some() || !stamp.clean.is_some_and(|c| same_transform(&c, tr)) {
        return false;
    }
    let same = fields_unchanged(same, t, eid, stamp);
    if same {
        arm(t);
    }
    same
}

/// The field-by-field tier of [`stamp_unchanged`]. Leaves the table disarmed:
/// setting its metatable aside is a write.
fn fields_unchanged(same: &Function, t: &Table, eid: u32, stamp: &NodeStamp) -> bool {
    // A frozen table cannot have its metatable set aside, and the full road
    // reports the frozen table exactly as it always did.
    if t.is_readonly() {
        return false;
    }
    let p = &stamp.pre;
    // Set aside and put back exactly as it was, whatever it is: the call
    // only reads.
    let mt = t.metatable();
    if mt.is_some() {
        t.set_metatable(None);
    }
    #[cfg(test)]
    let t0 = crate::host::STAMP_CHECK_TIMED.with(|c| c.get()).then(std::time::Instant::now);
    let same = same
        .call::<bool>((t, eid, p.x, p.y, p.z, p.sx, p.sy, p.sz, p.scale, p.yaw, p.pitch, p.roll))
        .unwrap_or(false);
    #[cfg(test)]
    if let Some(t0) = t0 {
        crate::host::STAMP_CHECK_NS.with(|c| c.set(c.get() + t0.elapsed().as_nanos() as u64));
    }
    if mt.is_some() {
        t.set_metatable(mt);
    }
    same
}

/// Test builds only: an armed table answered "unchanged", so the
/// field-by-field read must agree. A write that reached the table without
/// disarming it fails here, in every test that touches a kept table.
#[cfg(test)]
pub(crate) fn cross_check(same: &Function, t: &Table, eid: u32, stamp: &NodeStamp) {
    if NO_CROSS_CHECK.with(|c| c.get()) {
        return;
    }
    assert!(
        fields_unchanged(same, t, eid, stamp),
        "node {eid}'s table was still armed but no longer what the last stamp left: a write reached it without disarming it"
    );
    arm(t);
}

/// Snapshot the table's current values, to compare the next hook against.
pub(crate) fn node_stamp(t: &Table, tr: &Transform) -> NodeStamp {
    let mut pre = node_pre(tr);
    // Read from the table, not the transform: after a hook these differ whenever the
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
        clean: None,
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
///
/// **Raw reads, and this is a hot path.** [`stamp_node_table`] writes these ten
/// as direct fields before every hook, so the node metatable's `__index` can
/// never be reached for them — but on Luau an mlua `get` against a table that
/// merely has a metatable takes the protected path and allocates about 96 bytes
/// per key regardless. Ten keys, twice a pass, three passes a frame, on every
/// scripted node: that alone was ~4.5 KB of Lua heap per scripted node per
/// frame before a script did anything at all, and most of what a 0.84 profile
/// read as a game's own garbage. A key the script has explicitly nil'd reads as
/// its pre-hook value — which is what "the script did not change it" already
/// meant, and what `__index` answered anyway.
pub(crate) fn apply_node(t: &Table, tr: &mut Transform, pre: &NodePre) -> mlua::Result<()> {
    // `Option` rather than a bare fallback so a field the script set to a
    // non-number still raises, exactly as the metatable read did.
    let num = |k: &str, unchanged: f64| -> mlua::Result<f64> {
        Ok(t.raw_get::<Option<f64>>(k)?.unwrap_or(unchanged))
    };
    let x = num("x", pre.x)?;
    let y = num("y", pre.y)?;
    let z = num("z", pre.z)?;
    if x != pre.x || y != pre.y || z != pre.z {
        tr.translation = DVec3::new(x, y, z);
    }

    let scale = num("scale", pre.scale)?;
    if scale != pre.scale {
        tr.scale = Vec3::splat(scale as f32);
    } else {
        let sx = num("scale_x", pre.sx)?;
        let sy = num("scale_y", pre.sy)?;
        let sz = num("scale_z", pre.sz)?;
        if sx != pre.sx || sy != pre.sy || sz != pre.sz {
            tr.scale = Vec3::new(sx as f32, sy as f32, sz as f32);
        }
    }

    let yaw = num("yaw", pre.yaw)?;
    let pitch = num("pitch", pre.pitch)?;
    let roll = num("roll", pre.roll)?;
    if yaw != pre.yaw || pitch != pre.pitch || roll != pre.roll {
        tr.rotation = Quat::from_euler(EulerRot::YXZ, yaw as f32, pitch as f32, roll as f32);
    }
    Ok(())
}

/// Create a Lua **node handle** for entity index `e`: a table `{__id = e}` with the shared
/// node metatable, so `h.x`, `h.parent`, `h:getscript("foo")`, etc. work for any node.
/// The entity index behind a node handle, or `None` for anything else.
/// Deliberately tolerant of a plain `{__id = n}` table: user code passes those
/// around, and rejecting them would be a distinction without a difference.
pub(crate) fn node_id_of(v: &mlua::Value) -> Option<u32> {
    match v {
        mlua::Value::Table(t) => t.raw_get::<u32>("__id").ok(),
        _ => None,
    }
}

pub(crate) fn new_node_handle(lua: &Lua, e: u32) -> mlua::Result<Table> {
    let t = lua.create_table_with_capacity(0, 1)?;
    t.raw_set("__id", e)?;
    if let Some(book) = lua.app_data_ref::<NodeBook>() {
        let held = match book.by_id.raw_get::<Option<Table>>(e)? {
            Some(held) => held,
            None => {
                let held = lua.create_table()?;
                held.set_metatable(Some(book.weak_keys.clone()));
                book.by_id.raw_set(e, &held)?;
                held
            }
        };
        held.raw_set(&t, true)?;
    }
    // Held beside the state rather than looked up by name: `children()` on a
    // thousand-part craft makes a thousand of these, and the named lookup was
    // a registry string hash each.
    if let Some(mt) = lua.app_data_ref::<NodeMeta>() {
        t.set_metatable(Some(mt.0.clone()));
    } else if let Ok(mt) = lua.named_registry_value::<Table>("floptle_node_mt") {
        t.set_metatable(Some(mt));
    }
    Ok(t)
}

/// The node handle's metatable, kept as app data so making a handle is not a
/// registry lookup by name.
pub(crate) struct NodeMeta(pub(crate) Table);

/// The id a retired handle answers to: no node ever has it, so every read
/// through the handle finds nothing and every write reaches nothing, exactly
/// as for a node that has gone.
pub(crate) const RETIRED_NODE: u32 = u32::MAX;

/// Every node handle made, by node id, each set weak-keyed so it keeps no
/// handle alive. A handle names one node for that node's whole life: when
/// the node goes, or its slot is taken by a new node, every handle made for
/// it is retired, its `__id` replaced by [`RETIRED_NODE`] and the old id
/// kept in `__was` for `node.id`.
pub(crate) struct NodeBook {
    pub(crate) by_id: Table,
    pub(crate) weak_keys: Table,
}

/// Retire every handle made for the nodes at `ids`. See [`NodeBook`].
pub(crate) fn retire_node_handles(lua: &Lua, ids: &[u32]) {
    let Some(book) = lua.app_data_ref::<NodeBook>() else { return };
    for &id in ids {
        let Ok(Some(held)) = book.by_id.raw_get::<Option<Table>>(id) else { continue };
        for (h, _) in held.pairs::<Table, mlua::Value>().flatten() {
            let _ = h.raw_set("__was", id);
            let _ = h.raw_set("__id", RETIRED_NODE);
        }
        let _ = book.by_id.raw_set(id, mlua::Value::Nil);
    }
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

/// Create a Lua **sprite handle** for entity index `e`: a table `{__id}` with the
/// shared sprite metatable, so `sp.flipX`, `sp.cell` and the rest read and
/// assign like fields. Just the id — the numbers live in the scene mirror, so
/// two handles to one node can never disagree.
pub(crate) fn new_sprite_handle(lua: &Lua, e: u32) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.raw_set("__id", e)?;
    if let Ok(mt) = lua.named_registry_value::<Table>("floptle_sprite_mt") {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A tunable a script declares as a boolean must stay a boolean in `params`
    /// even after the Inspector stores it (params are floats on the way through).
    /// Every number is truthy in Lua, so a stored 0 arriving as `0` would make
    /// `if params.invert then` fire with the checkbox unticked.
    #[test]
    fn a_boolean_tunable_survives_the_float_round_trip() {
        let lua = Lua::new();
        let env = lua.create_table().unwrap();
        let defaults = lua.create_table().unwrap();
        defaults.set("invert", false).unwrap();
        defaults.set("speed", 4.0).unwrap();
        env.set("defaults", defaults).unwrap();

        // Nothing stored yet: the declared defaults come through as declared.
        let t = params_table(&lua, &env, &[], &[], &[]).unwrap();
        assert_eq!(t.get::<mlua::Value>("invert").unwrap(), mlua::Value::Boolean(false));

        // The Inspector ticked the box (stored as 1.0) and dragged the number.
        let t = params_table(&lua, &env, &[("invert".into(), 1.0), ("speed".into(), 6.5)], &[], &[])
            .unwrap();
        assert_eq!(t.get::<mlua::Value>("invert").unwrap(), mlua::Value::Boolean(true));
        assert_eq!(t.get::<f64>("speed").unwrap(), 6.5);

        // …and unticked again: a stored 0 must be `false`, not a truthy 0.
        let t = params_table(&lua, &env, &[("invert".into(), 0.0)], &[], &[]).unwrap();
        assert_eq!(t.get::<mlua::Value>("invert").unwrap(), mlua::Value::Boolean(false));
        let truthy: bool =
            lua.load("return ...").call(t.get::<mlua::Value>("invert").unwrap()).unwrap();
        assert!(!truthy, "`if params.invert then` must not fire for an unticked box");
    }
}
