//! The Lua `net.*` API + `synced` vars (`docs/netcode-design.md` §8) — the
//! script-facing face of `floptle-net`. Follows the host's queue-drain shape:
//! `net.host{}` / `net.rpc(...)` / `net.spawn(...)` queue [`NetCmd`]s the
//! editor drains each tick; session state (role/peers/ping) is mirrored IN via
//! [`NetState`]; received RPCs/events dispatch back through
//! `ScriptHost::dispatch_rpc` / `fire_net_event`.

use std::cell::RefCell;
use std::rc::Rc;

use floptle_net::{NetValue, ValueError};
use mlua::{Lua, RegistryKey, Table, Value};

use crate::{LogLevel, ScriptLog};

/// A queued session command from Lua, drained by the editor each tick.
#[derive(Clone, Debug)]
pub enum NetCmd {
    /// `net.host{ maxPlayers = n }` — become the authoritative host.
    Host { max_players: u32 },
    /// `net.join(addr)` — join a session (2b: `local://` only; real transports 2e).
    Join { addr: String },
    /// `net.leave()` — tear the session down.
    Leave,
    /// `net.rpc(name, args, { to = peer })` — a remote call (role decides direction).
    Rpc { name: String, args: NetValue, to: Option<u64> },
    /// `net.spawn(path, { x, y, z, owner })` — server-only replicated spawn.
    Spawn { path: String, pos: Option<[f64; 3]>, owner: Option<u64> },
    /// `net.despawn(node)` — server-only replicated despawn (entity index).
    Despawn { eid: u32 },
}

/// This endpoint's role, mirrored to Lua.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NetRoleState {
    #[default]
    Offline,
    Server,
    Client,
}

/// Live session state fed by the editor each tick, read by `net.role()` /
/// `net.peers()` / `net.ping()`.
#[derive(Clone, Debug, Default)]
pub struct NetState {
    pub role: NetRoleState,
    pub peers: Vec<u64>,
    pub rtt_ms: f32,
}

/// One `net.on(event, fn)` registration; owned by an `(entity, script)`
/// instance and dropped when its environment rebuilds (hot reload) or dies.
pub(crate) struct NetHandler {
    pub eid: u32,
    pub kind: String,
    pub event: String,
    pub key: RegistryKey,
}

/// Interior-mutable net state shared between the host and the `net.*` closures.
#[derive(Clone)]
pub(crate) struct SharedNet {
    pub cmds: Rc<RefCell<Vec<NetCmd>>>,
    pub state: Rc<RefCell<NetState>>,
    pub handlers: Rc<RefCell<Vec<NetHandler>>>,
    /// The `(entity, script)` currently executing (set around top-level exec +
    /// lifecycle calls) — how `net.on` knows who is registering.
    pub current: Rc<RefCell<Option<(u32, String)>>>,
    pub logs: Rc<RefCell<Vec<ScriptLog>>>,
}

impl SharedNet {
    pub fn new(logs: Rc<RefCell<Vec<ScriptLog>>>) -> Self {
        Self {
            cmds: Rc::new(RefCell::new(Vec::new())),
            state: Rc::new(RefCell::new(NetState::default())),
            handlers: Rc::new(RefCell::new(Vec::new())),
            current: Rc::new(RefCell::new(None)),
            logs,
        }
    }

    fn warn(&self, msg: String) {
        self.logs.borrow_mut().push(ScriptLog { level: LogLevel::Warn, msg, source: None });
    }
}

/// Convert a per-tick [`crate::InputSnapshot`] into the wire form (sorted for
/// deterministic encoding) — what a predicted node's owner ships to the server.
pub fn input_to_net(s: &crate::InputSnapshot) -> floptle_net::NetInput {
    let sorted = |set: &std::collections::HashSet<String>| {
        let mut v: Vec<String> = set.iter().cloned().collect();
        v.sort();
        v
    };
    floptle_net::NetInput {
        keys_down: sorted(&s.keys_down),
        keys_pressed: sorted(&s.keys_pressed),
        keys_released: sorted(&s.keys_released),
        mouse: s.mouse,
        mouse_delta: s.mouse_delta,
        scroll: s.scroll,
        buttons_down: s.buttons_down,
        buttons_pressed: s.buttons_pressed,
        aim: s.aim,
    }
}

/// The wire form back into a host input snapshot — what the server (and the
/// client's replay) feed `fixedUpdate` so the SAME controller runs on both
/// sides (`docs/netcode-design.md` §6, the one-script model).
pub fn net_to_input(n: &floptle_net::NetInput) -> crate::InputSnapshot {
    crate::InputSnapshot {
        keys_down: n.keys_down.iter().cloned().collect(),
        keys_pressed: n.keys_pressed.iter().cloned().collect(),
        keys_released: n.keys_released.iter().cloned().collect(),
        mouse: n.mouse,
        mouse_delta: n.mouse_delta,
        scroll: n.scroll,
        buttons_down: n.buttons_down,
        buttons_pressed: n.buttons_pressed,
        aim: n.aim,
    }
}

/// Convert a Lua value to a [`NetValue`], enforcing the §13.2 guardrails at
/// the boundary: functions/userdata/threads never replicate, depth ≤ 4, and
/// the caller validates encoded size. Errors carry a script-friendly message.
pub(crate) fn lua_to_netvalue(v: &Value, depth: usize) -> Result<NetValue, String> {
    if depth > floptle_net::MAX_VALUE_DEPTH {
        return Err(ValueError::TooDeep.to_string());
    }
    match v {
        Value::Nil => Ok(NetValue::Nil),
        Value::Boolean(b) => Ok(NetValue::Bool(*b)),
        Value::Integer(n) => Ok(NetValue::Num(*n as f64)),
        Value::Number(n) => Ok(NetValue::Num(*n)),
        Value::String(s) => Ok(NetValue::Str(s.to_string_lossy().to_string())),
        Value::Table(t) => {
            let mut pairs = Vec::new();
            for pair in t.clone().pairs::<Value, Value>() {
                let (k, val) = pair.map_err(|e| e.to_string())?;
                pairs.push((lua_to_netvalue(&k, depth + 1)?, lua_to_netvalue(&val, depth + 1)?));
            }
            Ok(NetValue::Table(pairs))
        }
        Value::Function(_) => Err("functions can't replicate".into()),
        Value::UserData(_) | Value::LightUserData(_) => Err("userdata can't replicate".into()),
        Value::Thread(_) => Err("coroutines can't replicate".into()),
        other => Err(format!("{} can't replicate", other.type_name())),
    }
}

/// Convert a received [`NetValue`] back into a Lua value.
pub(crate) fn netvalue_to_lua(lua: &Lua, v: &NetValue) -> mlua::Result<Value> {
    Ok(match v {
        NetValue::Nil => Value::Nil,
        NetValue::Bool(b) => Value::Boolean(*b),
        NetValue::Num(n) => Value::Number(*n),
        NetValue::Str(s) => Value::String(lua.create_string(s)?),
        NetValue::Table(pairs) => {
            let t = lua.create_table()?;
            for (k, val) in pairs {
                t.set(netvalue_to_lua(lua, k)?, netvalue_to_lua(lua, val)?)?;
            }
            Value::Table(t)
        }
    })
}

/// A Lua value converted + size/depth-validated, or a queued Console warning.
fn checked_netvalue(net: &SharedNet, ctx: &str, v: &Value) -> Option<NetValue> {
    match lua_to_netvalue(v, 0).and_then(|nv| {
        nv.validate().map_err(|e| e.to_string())?;
        Ok(nv)
    }) {
        Ok(nv) => Some(nv),
        Err(e) => {
            net.warn(format!("{ctx}: {e} — dropped"));
            None
        }
    }
}

/// Install the `net` global table.
pub(crate) fn install_net_api(lua: &Lua, net: &SharedNet) -> mlua::Result<()> {
    let t = lua.create_table()?;

    // --- session control -------------------------------------------------
    {
        let n = net.clone();
        t.set(
            "host",
            lua.create_function(move |_, opts: Option<Table>| {
                let max_players = opts
                    .and_then(|o| o.get::<Option<u32>>("maxPlayers").ok().flatten())
                    .unwrap_or(16);
                n.cmds.borrow_mut().push(NetCmd::Host { max_players });
                Ok(())
            })?,
        )?;
    }
    {
        let n = net.clone();
        t.set(
            "join",
            lua.create_function(move |_, addr: String| {
                n.cmds.borrow_mut().push(NetCmd::Join { addr });
                Ok(())
            })?,
        )?;
    }
    {
        let n = net.clone();
        t.set(
            "leave",
            lua.create_function(move |_, ()| {
                n.cmds.borrow_mut().push(NetCmd::Leave);
                Ok(())
            })?,
        )?;
    }

    // --- state -----------------------------------------------------------
    {
        let n = net.clone();
        t.set(
            "role",
            lua.create_function(move |_, ()| {
                Ok(match n.state.borrow().role {
                    NetRoleState::Offline => "offline",
                    NetRoleState::Server => "server",
                    NetRoleState::Client => "client",
                })
            })?,
        )?;
    }
    {
        let n = net.clone();
        t.set(
            "isServer",
            lua.create_function(move |_, ()| Ok(n.state.borrow().role == NetRoleState::Server))?,
        )?;
    }
    {
        let n = net.clone();
        t.set(
            "isClient",
            lua.create_function(move |_, ()| Ok(n.state.borrow().role == NetRoleState::Client))?,
        )?;
    }
    {
        let n = net.clone();
        t.set(
            "peers",
            lua.create_function(move |lua, ()| {
                let arr = lua.create_table()?;
                for (i, p) in n.state.borrow().peers.iter().enumerate() {
                    arr.set(i + 1, *p)?;
                }
                Ok(arr)
            })?,
        )?;
    }
    {
        let n = net.clone();
        t.set(
            "ping",
            lua.create_function(move |_, _peer: Option<u64>| Ok(n.state.borrow().rtt_ms))?,
        )?;
    }

    // --- rpc ---------------------------------------------------------------
    {
        let n = net.clone();
        t.set(
            "rpc",
            lua.create_function(move |_, (name, args, opts): (String, Option<Value>, Option<Table>)| {
                let Some(nv) =
                    checked_netvalue(&n, &format!("net.rpc(\"{name}\")"), &args.unwrap_or(Value::Nil))
                else {
                    return Ok(());
                };
                let to = opts.and_then(|o| o.get::<Option<u64>>("to").ok().flatten());
                n.cmds.borrow_mut().push(NetCmd::Rpc { name, args: nv, to });
                Ok(())
            })?,
        )?;
    }

    // --- events --------------------------------------------------------------
    {
        let n = net.clone();
        t.set(
            "on",
            lua.create_function(move |lua, (event, f): (String, mlua::Function)| {
                let Some((eid, kind)) = n.current.borrow().clone() else {
                    n.warn(format!("net.on(\"{event}\") outside a script — ignored"));
                    return Ok(());
                };
                let key = lua.create_registry_value(f)?;
                n.handlers.borrow_mut().push(NetHandler { eid, kind, event, key });
                Ok(())
            })?,
        )?;
    }

    // --- spawn / despawn -------------------------------------------------
    {
        let n = net.clone();
        t.set(
            "spawn",
            lua.create_function(move |_, (path, opts): (String, Option<Table>)| {
                if n.state.borrow().role != NetRoleState::Server {
                    n.warn(format!("net.spawn(\"{path}\"): only the server spawns — ignored"));
                    return Ok(());
                }
                let (mut pos, mut owner) = (None, None);
                if let Some(o) = opts {
                    let x = o.get::<Option<f64>>("x").ok().flatten();
                    let y = o.get::<Option<f64>>("y").ok().flatten();
                    let z = o.get::<Option<f64>>("z").ok().flatten();
                    if let (Some(x), Some(y), Some(z)) = (x, y, z) {
                        pos = Some([x, y, z]);
                    }
                    owner = o.get::<Option<u64>>("owner").ok().flatten();
                }
                n.cmds.borrow_mut().push(NetCmd::Spawn { path, pos, owner });
                Ok(())
            })?,
        )?;
    }
    {
        let n = net.clone();
        t.set(
            "despawn",
            lua.create_function(move |_, node: Table| {
                if n.state.borrow().role != NetRoleState::Server {
                    n.warn("net.despawn: only the server despawns — ignored".into());
                    return Ok(());
                }
                if let Ok(eid) = node.raw_get::<u32>("__id") {
                    n.cmds.borrow_mut().push(NetCmd::Despawn { eid });
                }
                Ok(())
            })?,
        )?;
    }

    lua.globals().set("net", t)
}

/// Build the per-instance `synced` proxy from a script's top-level
/// `replicated = { ... }` declaration: reads/writes land in a hidden store
/// table (returned, for host collection); on a CLIENT, writes warn — the
/// server owns these values and will overwrite them.
pub(crate) fn build_synced_proxy(
    lua: &Lua,
    net: &SharedNet,
    declared: &Table,
    kind: &str,
) -> mlua::Result<(Table, Table)> {
    let store = lua.create_table()?;
    for pair in declared.clone().pairs::<Value, Value>() {
        let (k, v) = pair?;
        store.set(k, v)?;
    }
    let proxy = lua.create_table()?;
    let mt = lua.create_table()?;
    mt.set("__index", store.clone())?;
    {
        let n = net.clone();
        let store = store.clone();
        let kind = kind.to_string();
        mt.set(
            "__newindex",
            lua.create_function(move |_, (_, k, v): (Table, Value, Value)| {
                if n.state.borrow().role == NetRoleState::Client {
                    let key = match &k {
                        Value::String(s) => s.to_string_lossy().to_string(),
                        other => format!("{other:?}"),
                    };
                    n.warn(format!(
                        "{kind}: synced.{key} written on a CLIENT — the server owns synced vars; this write will be overwritten"
                    ));
                }
                store.raw_set(k, v)
            })?,
        )?;
    }
    proxy.set_metatable(Some(mt));
    Ok((proxy, store))
}
