//! The Lua `net.*` API + `synced` vars (`docs/netcode-design.md` §8) — the
//! script-facing face of `floptle-net`. Follows the host's queue-drain shape:
//! `net.host{}` / `net.rpc(...)` / `net.spawn(...)` queue [`NetCmd`]s the
//! editor drains each tick; session state (role/peers/ping) is mirrored IN via
//! [`NetState`]; received RPCs/events dispatch back through
//! `ScriptHost::dispatch_rpc` / `fire_net_event`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use floptle_net::{NetValue, ValueError};
use mlua::{Lua, RegistryKey, Table, Value};

use crate::{LogLevel, ScriptLog};

/// A queued session command from Lua, drained by the editor each tick.
#[derive(Clone, Debug)]
pub enum NetCmd {
    /// `net.host{ maxPlayers = n, port = p, relay = "addr" }` — become the
    /// authoritative host. With a `relay`, host through a rendezvous relay
    /// (lobby code, no port-forwarding); with a `port`, a REAL session on UDP
    /// (QUIC) that other machines join with `net.join("quic://ip:port")`;
    /// with neither, the in-editor loopback harness.
    Host {
        max_players: u32,
        port: Option<u16>,
        relay: Option<String>,
        /// `interest = <metres>` — turn on interest management with that
        /// radius (`docs/netcode-design.md` §5.2). Absent = broadcast to
        /// everyone, which is the default and the right answer below a few
        /// dozen players.
        interest: Option<f64>,
        /// `interestBudget = <bytes per second>` — per-client snapshot budget.
        /// Only meaningful alongside `interest`.
        interest_budget: Option<u32>,
    },
    /// `net.join(addr)` — join a session (2b: `local://` only; real transports 2e).
    Join { addr: String },
    /// `net.leave()` — tear the session down.
    Leave,
    /// `net.rpc(name, args, { to = peer, withInput = bool })` — a remote call
    /// (role decides direction). `with_input` stamps the sender's perceived
    /// tick for lag compensation (§7) — client → server intents only.
    Rpc { name: String, args: NetValue, to: Option<u64>, with_input: bool },
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

/// Live ROLLBACK state fed by the driver each tick
/// (`docs/rollback-netcode-design.md` §7 P6), read by `net.rollbackDepth()` and
/// friends — and the per-tick seed behind `net.random()`.
#[derive(Clone, Copy, Debug, Default)]
pub struct RollbackInfo {
    /// Is a rollback session actually driving this scene?
    pub active: bool,
    /// The tick being simulated. Identical on every peer for a given tick, and
    /// identical on a replay of it — which is what makes it safe to seed from.
    pub tick: u64,
    /// The session's match seed, chosen once by the host and carried in
    /// `RollbackStart`.
    pub seed: u64,
    /// Ticks re-simulated by the most recent correction, and the worst so far.
    pub depth: u32,
    pub max_depth: u32,
    /// Mean ticks re-simulated per correction — the texture of the connection,
    /// where `max_depth` is only its worst moment.
    pub average_depth: f32,
    /// 0..1 — the fraction of simulated ticks that had to guess something.
    pub mispredict_rate: f32,
    /// The session's fixed input delay, ticks.
    pub input_delay: u8,
    /// Waiting for input rather than guessing past the depth cap.
    pub stalled: bool,
}

/// Live session state fed by the editor each tick, read by `net.role()` /
/// `net.peers()` / `net.ping()` / `net.isMine()`.
#[derive(Clone, Debug)]
pub struct NetState {
    pub role: NetRoleState,
    pub peers: Vec<u64>,
    pub rtt_ms: f32,
    /// Client: our peer id once welcomed (`net.isMine` needs it).
    pub my_peer: Option<u64>,
    /// Client: how the join attempt is going — `"connecting"`, `"joined"`, or
    /// `"refused"`, with `join_error` carrying the relay's own words.
    ///
    /// Needed because joining does not block: `role` reads `Client` from the
    /// frame `net.join` was called, whether or not that lobby exists.
    pub join_state: &'static str,
    /// Why a join was refused, when it was.
    pub join_error: Option<String>,
    /// Host, relay sessions only: the lobby code friends type in to join.
    ///
    /// The relay hands this back when the lobby registers, which is a moment
    /// only the engine sees — so without mirroring it here a game could ask for
    /// a relay session and then have no way to tell its own players the code.
    /// The 🌐 panel had it and a lobby screen did not, which meant every game
    /// shipping its own front end had to send players to an engine debug panel.
    /// `None` offline, on a client, and on a direct/LAN host (there is no code
    /// to show — joiners use the address).
    pub lobby_code: Option<String>,
}

impl Default for NetState {
    fn default() -> Self {
        Self {
            role: NetRoleState::Offline,
            peers: Vec::new(),
            rtt_ms: 0.0,
            my_peer: None,
            // Hand-written rather than derived: `&'static str` defaults to the
            // empty string, and a join state of "" is a third thing a game
            // would have to know about. With no session there is no join in
            // progress, and "offline" says exactly that.
            join_state: "offline",
            join_error: None,
            lobby_code: None,
        }
    }
}

/// One `net.on(event, fn)` registration; owned by an `(entity, script)`
/// instance and dropped when its environment rebuilds (hot reload) or dies.
pub(crate) struct NetHandler {
    pub eid: u32,
    pub kind: String,
    pub event: String,
    pub key: RegistryKey,
}

/// The lag-compensation context for the RPC currently being dispatched on the
/// server (`docs/netcode-design.md` §7): the world state at the tick the
/// SENDER perceived, precomputed by the driver from its history ring. Staged
/// via `ScriptHost::set_rewind` around `dispatch_rpc`; `net.rewind(peer, fn)`
/// applies it for the duration of `fn`.
#[derive(Clone, Debug, Default)]
pub struct RewindScope {
    /// The peer whose perceived time this is (the RPC's sender).
    pub peer: u64,
    /// (entity index, world position) per networked body at the rewound tick.
    pub poses: Vec<(u32, [f64; 3])>,
    /// (entity index, script kind, vars) — `synced` values at the rewound tick,
    /// so combat flags (parrying!) are judged at the SAME instant as the poses.
    pub synced: RewoundVars,
}

/// Per-entity rewound `synced` values: (entity index, script kind, name→value).
pub type RewoundVars = Vec<(u32, String, Vec<(String, NetValue)>)>;

/// Interior-mutable net state shared between the host and the `net.*` closures.
#[derive(Clone)]
pub(crate) struct SharedNet {
    pub cmds: Rc<RefCell<Vec<NetCmd>>>,
    pub state: Rc<RefCell<NetState>>,
    pub handlers: Rc<RefCell<Vec<NetHandler>>>,
    /// The `(entity, script)` currently executing (set around top-level exec +
    /// lifecycle calls) — how `net.on` knows who is registering.
    pub current: Rc<RefCell<Option<(u32, String)>>>,
    /// Lag-comp context for the RPC being dispatched (see [`RewindScope`]).
    pub rewind: Rc<RefCell<Option<RewindScope>>>,
    /// Networked nodes' owners (entity index → `Replicated::owner`), fed by
    /// the driver each tick — what `net.isMine(node)` answers from. Nodes not
    /// in the map aren't networked (local everywhere → always "mine").
    pub owners: Rc<RefCell<std::collections::HashMap<u32, Option<u64>>>>,
    pub logs: Rc<RefCell<Vec<ScriptLog>>>,
    /// Rollback diagnostics + the `net.random()` seed, refreshed per tick.
    pub rollback: Rc<std::cell::Cell<RollbackInfo>>,
    /// How many times `net.random()` has been called during the tick currently
    /// being simulated. Reset by the driver at the top of every tick — live and
    /// replayed alike — so a replayed tick draws exactly the same numbers the
    /// live tick drew.
    pub random_draws: Rc<std::cell::Cell<u64>>,
}

impl SharedNet {
    pub fn new(logs: Rc<RefCell<Vec<ScriptLog>>>) -> Self {
        Self {
            cmds: Rc::new(RefCell::new(Vec::new())),
            state: Rc::new(RefCell::new(NetState::default())),
            handlers: Rc::new(RefCell::new(Vec::new())),
            current: Rc::new(RefCell::new(None)),
            rewind: Rc::new(RefCell::new(None)),
            owners: Rc::new(RefCell::new(std::collections::HashMap::new())),
            logs,
            rollback: Rc::new(std::cell::Cell::new(RollbackInfo::default())),
            random_draws: Rc::new(std::cell::Cell::new(0)),
        }
    }

    fn warn(&self, msg: String) {
        self.logs.borrow_mut().push(ScriptLog { level: LogLevel::Warn, msg, source: None });
    }
}

/// A number in `[0, 1)` from (match seed, tick, draw index), by SplitMix64.
///
/// Every input is state every peer already agrees on, so every peer draws the
/// same value — and a re-simulation of the same tick, drawing in the same order,
/// draws it again. That last property is what a hand-rolled `rng(matchSeed +
/// tick)` usually misses: it re-seeds per tick but not per *draw*, so two calls
/// in one tick return the same number.
fn deterministic_unit(seed: u64, tick: u64, draw: u64) -> f64 {
    let mut z = seed
        .wrapping_add(tick.wrapping_mul(0x9E37_79B9_7F4A_7C15))
        .wrapping_add(draw.wrapping_mul(0xBF58_476D_1CE4_E5B9));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // 53 bits — the whole f64 mantissa, and no rounding to exactly 1.0.
    (z >> 11) as f64 / (1u64 << 53) as f64
}

/// A resolved tick of ACTIONS into the wire form — what a predicted node's
/// owner ships to the server.
///
/// The wire carries actions, not keys: a pad player and a keyboard player who
/// both press "Jump" produce byte-identical commands, so the one controller
/// script replays the same everywhere. `aim` rides along because a
/// camera-relative controller's view angle is genuinely part of the input.
pub fn input_to_net(
    state: &floptle_input::ActionState,
    aim: Option<[f32; 2]>,
) -> floptle_net::NetInput {
    floptle_net::NetInput {
        actions: state.held,
        just_pressed: state.just_pressed,
        just_released: state.just_released,
        axes1: state.axes1.clone(),
        axes2: state.axes2.clone(),
        aim,
    }
}

/// The wire form back into a resolved action state — what the server (and the
/// client's replay) feed `fixedUpdate` so the SAME controller runs on both
/// sides (`docs/netcode-design.md` §6, the one-script model).
///
/// `held_secs` is NOT transmitted: it's derivable and would cost 4 bytes per
/// action every tick. It is reconstructed by the receiver advancing its own
/// timer, which is exact as long as the tick stream is (and if it isn't, the
/// hold time is the least of the problems).
pub fn net_to_input(n: &floptle_net::NetInput, action_count: usize) -> floptle_input::ActionState {
    floptle_input::ActionState {
        held: n.actions,
        just_pressed: n.just_pressed,
        just_released: n.just_released,
        held_secs: vec![0.0; action_count],
        axes1: n.axes1.clone(),
        axes2: n.axes2.clone(),
    }
}

/// The `aim` a wire input carried, for feeding the legacy raw snapshot.
pub fn net_aim(n: &floptle_net::NetInput) -> Option<[f32; 2]> {
    n.aim
}

/// Convert a Lua value to a [`NetValue`], enforcing the §13.2 guardrails at
/// the boundary: functions/userdata/threads never replicate, depth ≤ 4, and
/// the caller validates encoded size. Errors carry a script-friendly message.
pub(crate) fn lua_to_netvalue(v: &Value, depth: usize) -> Result<NetValue, String> {
    lua_to_netvalue_max(v, depth, floptle_net::MAX_VALUE_DEPTH)
}

/// The same conversion with an explicit depth ceiling. Rollback state stays in
/// this process, so it isn't held to the wire's depth-4 rule — a controller's
/// state table is legitimately deeper than anything you'd replicate — but it
/// still needs A limit, or a cyclic table recurses until the stack goes.
pub(crate) fn lua_to_netvalue_max(
    v: &Value,
    depth: usize,
    max_depth: usize,
) -> Result<NetValue, String> {
    if depth > max_depth {
        return Err(if max_depth == floptle_net::MAX_VALUE_DEPTH {
            ValueError::TooDeep.to_string()
        } else {
            format!("value nests deeper than {max_depth} levels (or is cyclic)")
        });
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
                pairs.push((
                    lua_to_netvalue_max(&k, depth + 1, max_depth)?,
                    lua_to_netvalue_max(&val, depth + 1, max_depth)?,
                ));
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

/// Install the `net` global table. `hulls`/`sim_origin`/`synced_stores` are
/// the host's shared frame state — `net.rewind` re-poses the hulls and swaps
/// historical `synced` values in around a lag-compensated handler.
pub(crate) fn install_net_api(
    lua: &Lua,
    net: &SharedNet,
    hulls: &Rc<RefCell<Vec<floptle_physics::BodyHull>>>,
    sim_origin: &Rc<RefCell<glam::DVec3>>,
    synced_stores: &Rc<RefCell<std::collections::HashMap<(u32, String), Table>>>,
    replaying: &Rc<std::cell::Cell<bool>>,
) -> mlua::Result<()> {
    let t = lua.create_table()?;

    // --- rollback --------------------------------------------------------
    {
        // `net.replaying()` — true while the rollback driver is re-simulating
        // ticks it already ran (`docs/rollback-netcode-design.md` §4). The
        // engine already discards the side-effect queues a replay re-fires;
        // this is for the cosmetics it cannot see, like a script poking a
        // material or a UI label. Simulation code must NOT branch on it: a
        // replayed tick that computes something different from the live tick
        // is the definition of a desync.
        let r = replaying.clone();
        t.set("replaying", lua.create_function(move |_, ()| Ok(r.get()))?)?;
    }
    // The connection-quality readouts a fighting game actually cares about. A
    // fighter's netplay quality is rollback depth and how often the sim had to
    // guess — not ping, which says nothing about how the match feels.
    for (name, pick) in [
        ("rollbackDepth", 0u8),
        ("rollbackMax", 1),
        ("rollbackAverage", 2),
        ("mispredictRate", 3),
        ("inputDelay", 4),
    ] {
        let rb = net.rollback.clone();
        t.set(
            name,
            lua.create_function(move |_, ()| {
                let i = rb.get();
                Ok(match pick {
                    0 => i.depth as f64,
                    1 => i.max_depth as f64,
                    2 => i.average_depth as f64,
                    3 => i.mispredict_rate as f64,
                    _ => i.input_delay as f64,
                })
            })?,
        )?;
    }
    {
        // `net.stalled()` — the sim is waiting for input rather than guessing
        // past the depth cap. A game can show its own "connection trouble"
        // banner off this instead of leaving the player to wonder why the
        // match feels slow.
        let rb = net.rollback.clone();
        t.set("stalled", lua.create_function(move |_, ()| Ok(rb.get().stalled))?)?;
    }
    {
        // `net.random()` — the deterministic RNG (§3).
        //
        // `rng()` with no seed rolls from the clock, which is poison in a
        // rollback sim: two peers draw different numbers and the match forks.
        // This is seeded from (match seed, tick, draw index), so every peer
        // draws the same sequence for a tick, and a REPLAY of that tick draws
        // it again — which is the part a hand-rolled seed usually gets wrong.
        //
        // Shapes match Lua's own `math.random`: `net.random()` → [0,1),
        // `net.random(n)` → 1..n, `net.random(a, b)` → a..b (integers).
        let rb = net.rollback.clone();
        let draws = net.random_draws.clone();
        let logs = net.logs.clone();
        // Said ONCE per session, not once per call: a fighter that rolls a
        // number in `fixedUpdate` calls this 60 times a second, and a note
        // repeated 60 times a second is not a note, it is a broken console.
        let warned = Rc::new(Cell::new(false));
        t.set(
            "random",
            lua.create_function(move |_, (a, b): (Option<f64>, Option<f64>)| {
                let info = rb.get();
                if !info.active && !warned.replace(true) {
                    // Offline it still works — a single-player run has nothing
                    // to agree with — but it is worth saying once that the
                    // determinism guarantee is not in force.
                    logs.borrow_mut().push(ScriptLog {
                        level: LogLevel::Debug,
                        msg: "net.random() outside a rollback session — deterministic per \
                              tick, but there is no peer to agree with"
                            .into(),
                        source: None,
                    });
                }
                let n = draws.get();
                draws.set(n + 1);
                let unit = deterministic_unit(info.seed, info.tick, n);
                Ok(match (a, b) {
                    (None, _) => unit,
                    (Some(hi), None) => (1.0 + unit * hi.max(1.0)).floor().min(hi.max(1.0)),
                    (Some(lo), Some(hi)) => {
                        let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
                        (lo + unit * (hi - lo + 1.0)).floor().min(hi)
                    }
                })
            })?,
        )?;
    }

    // --- session control -------------------------------------------------
    {
        let n = net.clone();
        t.set(
            "host",
            lua.create_function(move |_, opts: Option<Table>| {
                let (mut max_players, mut port, mut relay) = (16, None, None);
                let (mut interest, mut interest_budget) = (None, None);
                if let Some(o) = opts {
                    max_players =
                        o.get::<Option<u32>>("maxPlayers").ok().flatten().unwrap_or(16);
                    port = o.get::<Option<u16>>("port").ok().flatten();
                    relay = o.get::<Option<String>>("relay").ok().flatten();
                    interest = o.get::<Option<f64>>("interest").ok().flatten();
                    interest_budget = o.get::<Option<u32>>("interestBudget").ok().flatten();
                }
                n.cmds.borrow_mut().push(NetCmd::Host {
                    max_players,
                    port,
                    relay,
                    interest,
                    interest_budget,
                });
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
    // net.joinState() — "offline" | "connecting" | "joined" | "refused".
    // A lobby screen should wait on THIS rather than on net.role(): joining
    // does not block, so role says "client" from the frame you called join,
    // whether or not the code was real. Second return is the reason on
    // "refused" — the relay's own words, e.g. "no lobby QK7RM".
    {
        let n = net.clone();
        t.set(
            "joinState",
            lua.create_function(move |_, ()| {
                let st = n.state.borrow();
                Ok((st.join_state.to_string(), st.join_error.clone()))
            })?,
        )?;
    }
    // net.lobbyCode() — the code friends type in, on a relay host. nil until
    // the relay answers (a lobby screen should poll rather than read once), and
    // nil for good on a client or a direct/LAN host, where there is no code.
    {
        let n = net.clone();
        t.set(
            "lobbyCode",
            lua.create_function(move |_, ()| Ok(n.state.borrow().lobby_code.clone()))?,
        )?;
    }
    // net.isMine(node): is this node under MY control on this machine?
    // Offline / non-networked → true. On the server: true unless a remote
    // peer owns it. On a client: true only for my own predicted node(s).
    // THE way for shared scripts (cameras, HUDs) to pick the local player
    // out of many identical avatars.
    {
        let n = net.clone();
        t.set(
            "isMine",
            lua.create_function(move |_, node: Table| {
                let Ok(eid) = node.raw_get::<u32>("__id") else { return Ok(false) };
                let owner = n.owners.borrow().get(&eid).copied();
                Ok(match (n.state.borrow().role, owner) {
                    (_, None) => true, // not networked: local everywhere
                    (NetRoleState::Offline, _) => true,
                    (NetRoleState::Server, Some(o)) => o.is_none_or(|p| p == 0),
                    (NetRoleState::Client, Some(o)) => {
                        o.is_some() && o == n.state.borrow().my_peer
                    }
                })
            })?,
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
                let (mut to, mut with_input) = (None, false);
                if let Some(o) = opts {
                    to = o.get::<Option<u64>>("to").ok().flatten();
                    with_input =
                        o.get::<Option<bool>>("withInput").ok().flatten().unwrap_or(false);
                }
                n.cmds.borrow_mut().push(NetCmd::Rpc { name, args: nv, to, with_input });
                Ok(())
            })?,
        )?;
    }

    // --- lag-compensated queries (§7) --------------------------------------
    // net.rewind(peer, fn): inside `fn`, raycasts see the networked bodies
    // where `peer` PERCEIVED them (their interp-delayed view at the tick their
    // rpc was stamped with), and other scripts' `synced` vars read the values
    // from that same tick. Only meaningful on the server, inside an `onRpc`
    // handler for an rpc sent `{withInput = true}` — anywhere else it warns
    // and runs `fn` at server time (the honest fallback).
    {
        let n = net.clone();
        let hulls = hulls.clone();
        let so = sim_origin.clone();
        let stores = synced_stores.clone();
        t.set(
            "rewind",
            lua.create_function(move |lua, (peer, f): (u64, mlua::Function)| {
                let scope = n.rewind.borrow().clone();
                let scope = match scope {
                    Some(s) if s.peer == peer => s,
                    Some(s) => {
                        n.warn(format!(
                            "net.rewind({peer}): the current rpc was sent by peer {} — \
                             rewinding to ITS view; queries run at server time instead",
                            s.peer
                        ));
                        return f.call::<mlua::MultiValue>(());
                    }
                    None => {
                        n.warn(
                            "net.rewind: no lag-comp context — call it on the SERVER inside an \
                             onRpc handler for an rpc sent {withInput = true}; queries run at \
                             server time instead"
                                .into(),
                        );
                        return f.call::<mlua::MultiValue>(());
                    }
                };
                // Re-pose the hulls to the rewound tick (world → sim frame).
                let origin = *so.borrow();
                let mut saved_poses = Vec::new();
                {
                    let mut hs = hulls.borrow_mut();
                    for (eid, wpos) in &scope.poses {
                        for h in hs.iter_mut().filter(|h| h.eid == *eid) {
                            saved_poses.push((*eid, h.pos));
                            h.pos = (glam::DVec3::from_array(*wpos) - origin).as_vec3();
                        }
                    }
                }
                // Swap the rewound synced values in (saving the live ones).
                let mut saved_vars: Vec<(Table, String, Value)> = Vec::new();
                {
                    let stores = stores.borrow();
                    for (eid, kind, vars) in &scope.synced {
                        let Some(store) = stores.get(&(*eid, kind.clone())) else { continue };
                        for (k, v) in vars {
                            let Ok(hist) = netvalue_to_lua(lua, v) else { continue };
                            let cur = store.raw_get::<Value>(k.as_str()).unwrap_or(Value::Nil);
                            saved_vars.push((store.clone(), k.clone(), cur));
                            let _ = store.raw_set(k.as_str(), hist);
                        }
                    }
                }
                let result = f.call::<mlua::MultiValue>(());
                // Restore the present — even when the handler errored.
                for (store, k, v) in saved_vars {
                    let _ = store.raw_set(k.as_str(), v);
                }
                {
                    let mut hs = hulls.borrow_mut();
                    for (eid, pos) in saved_poses {
                        if let Some(h) = hs.iter_mut().find(|h| h.eid == eid) {
                            h.pos = pos;
                        }
                    }
                }
                result
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
