//! The Lua `assembly.*` API — compound-rigidbody (multi-part vessel) control.
//!
//! An ASSEMBLY is a node whose RigidBody has the `assembly` flag: one 6-DOF
//! compound body built from its RigidBody-bearing descendant nodes (see
//! `floptle-physics::compound`). Ships, rovers, cranes — anything built from
//! parts that can also come apart.
//!
//! Forces QUEUE and the editor feeds them to the sim as tick-held forces (they
//! act through every physics substep of the tick, then clear — scripts re-arm
//! thrust each `fixedUpdate`, and a dropped call means thrust stops). Reads
//! (`assembly.info`) come from a per-frame mirror the editor refreshes before
//! scripts run. `assembly.split` queues a detach the editor performs after the
//! script pass: it spawns a fresh root node, re-parents the detached part
//! nodes, splits the physics compound, and calls your callback with the new
//! vessel's node.
//!
//! World-space everywhere; vectors are `vec3(...)` values (or any `{x,y,z}`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use mlua::{Lua, RegistryKey, Table, Value};

/// Mirrored per-assembly state, refreshed by the editor each frame.
#[derive(Clone, Debug, Default)]
pub struct AssemblyInfo {
    pub mass: f32,
    /// Center of mass, WORLD space.
    pub com: [f64; 3],
    pub vel: [f32; 3],
    pub ang_vel: [f32; 3],
    pub grounded: bool,
    /// Pinned in place by `assembly.setAnchored` (launch clamps, latches).
    pub anchored: bool,
    /// Part entity indices (the compound's shape ids).
    pub parts: Vec<u32>,
}

/// One queued `assembly.*` command, drained by the editor after the script pass.
pub enum AssemblyCmd {
    /// Hold `force` at world point `at` (`None` = through the CoM) plus a pure
    /// `torque` for the next tick.
    Hold { root: u32, force: [f64; 3], at: Option<[f64; 3]>, torque: [f64; 3] },
    /// Instantaneous impulse at a world point.
    Impulse { root: u32, imp: [f64; 3], at: [f64; 3] },
    /// Detach `parts` (entity indices) into a new vessel; `cb` (if any) is
    /// called with the new root's node table.
    Split { root: u32, parts: Vec<u32>, cb: Option<RegistryKey> },
    /// Re-gather the compound from the root's current descendants — call
    /// after script-assembling a vessel (spawning parts under the root).
    Rebuild { root: u32 },
    /// Pin the compound where it stands / release it (launch clamps).
    Anchor { root: u32, on: bool },
    /// Teleport the assembly ORIGIN to a world position, velocity untouched
    /// (re-pinning a clamped vessel to a pad that rides an orbiting planet).
    Teleport { root: u32, pos: [f64; 3] },
}

/// A `vec3(...)`-ish argument: any table with `x`/`y`/`z` fields.
fn v3(v: &Value, what: &str) -> mlua::Result<[f64; 3]> {
    if let Value::Table(t) = v {
        let (x, y, z) = (t.get::<f64>("x"), t.get::<f64>("y"), t.get::<f64>("z"));
        if let (Ok(x), Ok(y), Ok(z)) = (x, y, z) {
            return Ok([x, y, z]);
        }
    }
    Err(mlua::Error::runtime(format!("{what}: expected a vec3 (a table with x, y, z)")))
}

/// The `node` argument's entity index (a node table or handle).
fn node_eid(v: &Value, what: &str) -> mlua::Result<u32> {
    if let Value::Table(t) = v
        && let Ok(id) = t.raw_get::<u32>("__id")
    {
        return Ok(id);
    }
    Err(mlua::Error::runtime(format!("{what}: pass the assembly root node")))
}

fn vec3_table(lua: &Lua, v: [f64; 3]) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("x", v[0])?;
    t.set("y", v[1])?;
    t.set("z", v[2])?;
    Ok(t)
}

pub(crate) fn install_assembly_api(
    lua: &Lua,
    info: Rc<RefCell<HashMap<u32, AssemblyInfo>>>,
    cmds: Rc<RefCell<Vec<AssemblyCmd>>>,
) -> mlua::Result<()> {
    let t = lua.create_table()?;

    // assembly.forceAt(node, force, at) — hold a world-space force at a world
    // point for this tick (engines, RCS thrusters: the off-center part torques
    // the vessel). Re-arm every fixedUpdate while burning.
    {
        let q = cmds.clone();
        let f = lua.create_function(move |_, (node, force, at): (Value, Value, Value)| {
            let root = node_eid(&node, "assembly.forceAt(node, force, at)")?;
            let force = v3(&force, "assembly.forceAt: force")?;
            let at = v3(&at, "assembly.forceAt: at")?;
            q.borrow_mut().push(AssemblyCmd::Hold { root, force, at: Some(at), torque: [0.0; 3] });
            Ok(())
        })?;
        t.set("forceAt", f)?;
    }
    // assembly.force(node, force) — held force through the center of mass.
    {
        let q = cmds.clone();
        let f = lua.create_function(move |_, (node, force): (Value, Value)| {
            let root = node_eid(&node, "assembly.force(node, force)")?;
            let force = v3(&force, "assembly.force: force")?;
            q.borrow_mut().push(AssemblyCmd::Hold { root, force, at: None, torque: [0.0; 3] });
            Ok(())
        })?;
        t.set("force", f)?;
    }
    // assembly.torque(node, t) — held pure torque (reaction wheels, SAS).
    {
        let q = cmds.clone();
        let f = lua.create_function(move |_, (node, torque): (Value, Value)| {
            let root = node_eid(&node, "assembly.torque(node, t)")?;
            let torque = v3(&torque, "assembly.torque: t")?;
            q.borrow_mut().push(AssemblyCmd::Hold {
                root,
                force: [0.0; 3],
                at: None,
                torque,
            });
            Ok(())
        })?;
        t.set("torque", f)?;
    }
    // assembly.impulseAt(node, impulse, at) — one-shot kick at a world point
    // (separation springs, explosions, docking bumps).
    {
        let q = cmds.clone();
        let f = lua.create_function(move |_, (node, imp, at): (Value, Value, Value)| {
            let root = node_eid(&node, "assembly.impulseAt(node, impulse, at)")?;
            let imp = v3(&imp, "assembly.impulseAt: impulse")?;
            let at = v3(&at, "assembly.impulseAt: at")?;
            q.borrow_mut().push(AssemblyCmd::Impulse { root, imp, at });
            Ok(())
        })?;
        t.set("impulseAt", f)?;
    }
    // assembly.split(node, parts [, fn]) — detach part nodes (a node or a list
    // of nodes) into a NEW vessel. The detach happens after this script pass;
    // fn(newRoot) is called with the fresh vessel's node when it exists.
    {
        let q = cmds.clone();
        let f = lua.create_function(
            move |lua, (node, parts, cb): (Value, Value, Option<mlua::Function>)| {
                let root = node_eid(&node, "assembly.split(node, parts)")?;
                let mut eids = Vec::new();
                match &parts {
                    Value::Table(list) if list.raw_get::<u32>("__id").is_err() => {
                        for v in list.sequence_values::<Value>() {
                            eids.push(node_eid(&v?, "assembly.split: parts entry")?);
                        }
                    }
                    other => eids.push(node_eid(other, "assembly.split: parts")?),
                }
                if eids.is_empty() {
                    return Err(mlua::Error::runtime("assembly.split: no parts given"));
                }
                let cb = match cb {
                    Some(f) => Some(lua.create_registry_value(f)?),
                    None => None,
                };
                q.borrow_mut().push(AssemblyCmd::Split { root, parts: eids, cb });
                Ok(())
            },
        )?;
        t.set("split", f)?;
    }
    // assembly.rebuild(node) — re-gather the compound from the root's CURRENT
    // part children. Call once after spawning parts under an assembly root
    // (script-assembled vessels: blueprint spawners, procgen structures).
    {
        let q = cmds.clone();
        let f = lua.create_function(move |_, node: Value| {
            let root = node_eid(&node, "assembly.rebuild(node)")?;
            q.borrow_mut().push(AssemblyCmd::Rebuild { root });
            Ok(())
        })?;
        t.set("rebuild", f)?;
    }
    // assembly.setAnchored(node, on) — pin the vessel where it stands (launch
    // clamps, docking latches, construction holds): no gravity, no contacts,
    // velocities zero. setAnchored(node, false) releases it from rest.
    {
        let q = cmds.clone();
        let f = lua.create_function(move |_, (node, on): (Value, bool)| {
            let root = node_eid(&node, "assembly.setAnchored(node, on)")?;
            q.borrow_mut().push(AssemblyCmd::Anchor { root, on });
            Ok(())
        })?;
        t.set("setAnchored", f)?;
    }
    // assembly.teleport(node, pos) — move the assembly origin to a world
    // position without touching velocity. The compound writeback owns the
    // root node's transform, so plain node position writes are overwritten —
    // this is THE way to place a live assembly (pad pinning, save restores).
    {
        let q = cmds.clone();
        let f = lua.create_function(move |_, (node, pos): (Value, Value)| {
            let root = node_eid(&node, "assembly.teleport(node, pos)")?;
            let pos = v3(&pos, "assembly.teleport: pos")?;
            q.borrow_mut().push(AssemblyCmd::Teleport { root, pos });
            Ok(())
        })?;
        t.set("teleport", f)?;
    }
    // assembly.info(node) — mass, com (world vec3), vel, angVel (vec3 tables),
    // grounded, anchored, and parts (the part nodes' entity ids). nil for
    // non-assemblies.
    {
        let info = info.clone();
        let f = lua.create_function(move |lua, node: Value| {
            let root = node_eid(&node, "assembly.info(node)")?;
            let map = info.borrow();
            let Some(i) = map.get(&root) else { return Ok(Value::Nil) };
            let out = lua.create_table()?;
            out.set("mass", i.mass as f64)?;
            out.set("com", vec3_table(lua, i.com)?)?;
            out.set(
                "vel",
                vec3_table(lua, [i.vel[0] as f64, i.vel[1] as f64, i.vel[2] as f64])?,
            )?;
            out.set(
                "angVel",
                vec3_table(lua, [i.ang_vel[0] as f64, i.ang_vel[1] as f64, i.ang_vel[2] as f64])?,
            )?;
            out.set("grounded", i.grounded)?;
            out.set("anchored", i.anchored)?;
            let parts = lua.create_table()?;
            for (k, p) in i.parts.iter().enumerate() {
                parts.set(k + 1, *p)?;
            }
            out.set("parts", parts)?;
            Ok(Value::Table(out))
        })?;
        t.set("info", f)?;
    }

    lua.globals().set("assembly", t)
}
