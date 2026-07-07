//! `NetSession` — one endpoint of a running multiplayer session
//! (`docs/netcode-design.md` §9). The driver (editor play loop, later the
//! headless runtime) owns one per world and calls it once per gameplay tick:
//!
//! - **Server**: [`NetSession::tick_server`] AFTER physics — polls the
//!   transport (joins/leaves/RPCs), then at the snapshot cadence sends changed
//!   transforms + `synced` vars to every client (periodic keyframes heal loss).
//! - **Client**: [`NetSession::tick_client`] — polls (welcome/spawns/snapshots/
//!   RPCs), buffers snapshot samples, and writes interpolated transforms into
//!   the world a fixed delay behind the newest server tick.
//!
//! v1 scope (phase 2b): server-authoritative replication only — prediction
//! (2c) and lag compensation (2d) layer on top of exactly these seams.

use std::collections::{HashMap, VecDeque};

use floptle_core::math::{DVec3, Quat};
use floptle_core::transform::Transform;
use floptle_core::{Entity, Replicated, World};

use crate::predict::PredictedState;
use crate::transport::{Channel, Incoming, LinkStats, PeerId, Transport, SERVER};
use crate::value::NetValue;
use crate::wire::{InputCmd, Msg, NetInput, SnapEntry, SyncedEntry, PROTO_VERSION};

/// Which side of the wire this session is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetRole {
    Server,
    Client,
}

/// Session happenings for the game layer (`net.on(...)` in Lua).
#[derive(Clone, Debug, PartialEq)]
pub enum NetEvent {
    /// Client: the server accepted us.
    Connected,
    /// Client: the server refused / went away.
    Disconnected(String),
    /// A player joined (server: transport-level; client: relayed).
    PeerJoined(PeerId),
    PeerLeft(PeerId),
}

/// Where an outgoing RPC goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RpcTarget {
    /// Client → server (the only legal client target).
    Server,
    /// Server → every connected client.
    All,
    /// Server → one client.
    Peer(PeerId),
}

/// A received remote call, ready for `onRpc` dispatch. `sender` is stamped by
/// the receiving side's transport identity — a client can't spoof it. `tick`
/// (client → server, `{withInput = true}`) is the server tick the sender
/// PERCEIVED when firing — what `net.rewind` rewinds combat queries to (§7).
#[derive(Clone, Debug)]
pub struct ReceivedRpc {
    pub name: String,
    pub args: NetValue,
    pub sender: PeerId,
    pub tick: Option<u64>,
}

/// Per-entity `synced` script vars: (entity, script kind, name→value pairs).
pub type SyncedVars = Vec<(Entity, String, Vec<(String, NetValue)>)>;

/// Per-entity live physics state fed by the driver each tick (velocity +
/// grounded), so physics-synced snapshot entries carry what prediction needs.
pub type BodyStates = Vec<(Entity, [f32; 3], bool)>;

/// How many recent input ticks ride in every input packet (redundancy: a lost
/// packet doesn't lose a tick's input — later packets re-carry it). Inputs are
/// tiny, so the window is deep: an input only goes missing if this many
/// CONSECUTIVE packets all drop (0.5^10 ≈ 0.1% per tick at 50% loss) — and a
/// missing input is a guaranteed visible correction, so depth is cheap
/// insurance.
const INPUT_WINDOW: usize = 10;
/// Server-side per-peer input backlog cap, ticks.
const INPUT_BUFFER_CAP: usize = 64;

/// Client-side per-entity snapshot history for interpolation.
struct InterpBuf {
    samples: VecDeque<(u64, [f64; 3], [f32; 4])>,
    interp: bool,
    /// Per-node render delay behind the newest server tick, in ticks
    /// ([`Replicated::interp_delay`] — tweakable on the Networked component).
    delay: u64,
}

impl InterpBuf {
    fn new(rep: &Replicated) -> Self {
        Self { samples: VecDeque::new(), interp: rep.interp, delay: rep.interp_delay as u64 }
    }
}

const MAX_SAMPLES: usize = 32;
/// Default: snapshot every 2 ticks (30 Hz at the 60 Hz tick).
const SNAPSHOT_EVERY: u8 = 2;
/// A keyframe (full state) every N snapshots — heals unreliable-channel loss.
const KEYFRAME_EVERY: u32 = 30;

pub struct NetSession {
    role: NetRole,
    transport: Box<dyn Transport>,
    // --- identity ---
    net_to_ent: HashMap<u64, Entity>,
    ent_to_net: HashMap<Entity, u64>,
    next_id: u64,
    // --- server ---
    peers: Vec<PeerId>,
    last_sent: HashMap<u64, ([f64; 3], [f32; 4])>,
    last_synced: HashMap<(u64, String, String), NetValue>,
    /// Current synced values, refreshed by the driver each tick (diffed here).
    synced_now: SyncedVars,
    /// Runtime spawns alive right now, for late-joiner catch-up.
    spawned_docs: HashMap<u64, (String, Option<PeerId>)>,
    snap_count: u32,
    /// Live body states fed by the driver each tick (velocity + grounded per
    /// physics-synced entity) — carried in snapshots for prediction.
    body_states: HashMap<Entity, ([f32; 3], bool)>,
    /// Server diagnostics: ticks where a peer's exact input hadn't arrived and
    /// repeat-last was used. Nonzero while moving = clock skew too tight
    /// (mispredictions on the owner) — the harness surfaces it.
    late_inputs: u64,
    /// Per-peer received input commands, keyed by tick (prediction §6: the
    /// server replays the OWNER's real input through the same script).
    peer_inputs: HashMap<PeerId, VecDeque<InputCmd>>,
    /// Per-peer last input actually used — the repeat-last fallback when a
    /// tick's command hasn't arrived (late/lost).
    last_input: HashMap<PeerId, NetInput>,
    // --- client ---
    connected: bool,
    /// Our peer id, from `Welcome` (None until connected).
    my_peer: Option<PeerId>,
    /// The server tick stamped in `Welcome` — the client's first fix on the
    /// server's clock (real links run two independent tick clocks).
    welcome_tick: Option<u64>,
    /// Added to every outgoing input tick stamp: translates the client's LOCAL
    /// tick numbering into the server's, plus a lead margin so inputs arrive
    /// before the server simulates their tick. 0 on the in-editor harness
    /// (client and hidden server share one clock). Set by the driver once the
    /// link's RTT is known; authoritative states translate back via
    /// [`Self::input_stamp_offset`].
    stamp_offset: i64,
    interp: HashMap<u64, InterpBuf>,
    latest_server_tick: u64,
    /// Outgoing input window (last few ticks, resent redundantly).
    input_window: VecDeque<InputCmd>,
    /// Authoritative states received for OUR OWN predicted node, for the
    /// driver's reconcile step: (entity, server tick, state).
    predicted_in: Vec<(Entity, u64, PredictedState)>,
    // --- both ---
    events: Vec<NetEvent>,
    rpcs_in: Vec<ReceivedRpc>,
    /// Queued outgoing RPCs; the `Option<u64>` is the perceived-tick stamp
    /// (`withInput` on a client — captured at queue time, when the caller's
    /// view of the world is exactly what it acted on).
    rpcs_out: Vec<(RpcTarget, String, NetValue, Option<u64>)>,
    synced_in: SyncedVars,
}

impl NetSession {
    pub fn server(transport: Box<dyn Transport>) -> Self {
        Self::new(NetRole::Server, transport)
    }

    /// Client: says hello immediately; `Connected` arrives via events once the
    /// server welcomes us.
    pub fn client(mut transport: Box<dyn Transport>) -> Self {
        transport.send(SERVER, Channel::Reliable, &Msg::Hello { proto: PROTO_VERSION }.encode());
        Self::new(NetRole::Client, transport)
    }

    fn new(role: NetRole, transport: Box<dyn Transport>) -> Self {
        Self {
            role,
            transport,
            net_to_ent: HashMap::new(),
            ent_to_net: HashMap::new(),
            next_id: 1,
            peers: Vec::new(),
            last_sent: HashMap::new(),
            last_synced: HashMap::new(),
            synced_now: Vec::new(),
            spawned_docs: HashMap::new(),
            snap_count: 0,
            body_states: HashMap::new(),
            late_inputs: 0,
            peer_inputs: HashMap::new(),
            last_input: HashMap::new(),
            connected: false,
            my_peer: None,
            welcome_tick: None,
            stamp_offset: 0,
            interp: HashMap::new(),
            latest_server_tick: 0,
            input_window: VecDeque::new(),
            predicted_in: Vec::new(),
            events: Vec::new(),
            rpcs_in: Vec::new(),
            rpcs_out: Vec::new(),
            synced_in: Vec::new(),
        }
    }

    pub fn role(&self) -> NetRole {
        self.role
    }

    /// Client: has the server welcomed us yet?
    pub fn is_connected(&self) -> bool {
        self.role == NetRole::Server || self.connected
    }

    /// Server: currently connected client peers.
    pub fn peers(&self) -> &[PeerId] {
        &self.peers
    }

    pub fn stats(&self, peer: PeerId) -> LinkStats {
        self.transport.stats(peer)
    }

    /// Assign deterministic ids to the scene-authored `Replicated` nodes. Both
    /// sides call this at session start on the SAME scene (`docs/netcode-design.md`
    /// §4.1). Iterates in NODE order (the `Transform` column — the order
    /// `spawn_into`/`to_doc` write nodes), NOT `Replicated`-insertion order:
    /// a Networked component added in the Inspector mid-session lands at an
    /// arbitrary point in its own column, but node order round-trips the doc.
    pub fn register_scene(&mut self, world: &World) {
        let nodes: Vec<Entity> = world.query::<Transform>().map(|(e, _)| e).collect();
        for e in nodes {
            let Some(rep) = world.get::<Replicated>(e) else { continue };
            let id = self.next_id;
            self.next_id += 1;
            self.net_to_ent.insert(id, e);
            self.ent_to_net.insert(e, id);
            if self.role == NetRole::Client {
                self.interp.insert(id, InterpBuf::new(rep));
            }
        }
    }

    /// The entity a `NetId` maps to locally (if it exists here).
    pub fn entity_of(&self, id: u64) -> Option<Entity> {
        self.net_to_ent.get(&id).copied()
    }

    /// The `NetId` an entity replicates as (if it's networked + registered).
    pub fn net_id_of(&self, e: Entity) -> Option<u64> {
        self.ent_to_net.get(&e).copied()
    }

    /// Every registered `(NetId, entity)` pair — what the lag-comp history
    /// records each server tick.
    pub fn net_entities(&self) -> impl Iterator<Item = (u64, Entity)> + '_ {
        self.net_to_ent.iter().map(|(&id, &e)| (id, e))
    }

    /// Queue an outgoing RPC. Guardrails apply: an over-limit value is dropped
    /// whole with an error string returned (surface it in the Console).
    pub fn send_rpc(
        &mut self,
        name: &str,
        args: NetValue,
        target: RpcTarget,
    ) -> Result<(), String> {
        self.send_rpc_stamped(name, args, target, false)
    }

    /// [`Self::send_rpc`] with the `{withInput = true}` option: on a CLIENT the
    /// call is stamped with the newest server tick this session had applied —
    /// the tick whose (interp-delayed) world the player was looking at when
    /// they acted. The server hands it to lag compensation (§7). On a server
    /// the flag is a no-op (its view IS the authority).
    pub fn send_rpc_stamped(
        &mut self,
        name: &str,
        args: NetValue,
        target: RpcTarget,
        with_input: bool,
    ) -> Result<(), String> {
        args.validate().map_err(|e| format!("net.rpc(\"{name}\"): {e}"))?;
        let stamp = (with_input && self.role == NetRole::Client && self.latest_server_tick > 0)
            .then_some(self.latest_server_tick);
        self.rpcs_out.push((target, name.to_string(), args, stamp));
        Ok(())
    }

    /// Received RPCs since the last drain, for `onRpc` dispatch.
    pub fn take_rpcs(&mut self) -> Vec<ReceivedRpc> {
        std::mem::take(&mut self.rpcs_in)
    }

    /// Session events since the last drain, for `net.on` dispatch.
    pub fn take_events(&mut self) -> Vec<NetEvent> {
        std::mem::take(&mut self.events)
    }

    /// Client: received `synced` var updates (entity, script kind, changed vars).
    pub fn take_synced(&mut self) -> SyncedVars {
        std::mem::take(&mut self.synced_in)
    }

    /// Server: refresh the current `synced` values (the driver collects them from
    /// the script layer each tick; the session diffs + sends at snapshot time).
    pub fn update_synced(&mut self, values: SyncedVars) {
        self.synced_now = values;
    }

    /// Server: refresh live body states (velocity + grounded per physics-synced
    /// entity) — carried in snapshot entries so owners can reconcile predictions.
    pub fn update_body_states(&mut self, states: BodyStates) {
        self.body_states = states.into_iter().map(|(e, v, g)| (e, (v, g))).collect();
    }

    /// Client: our peer id, once welcomed.
    pub fn my_peer(&self) -> Option<PeerId> {
        self.my_peer
    }

    /// Client: the server tick carried by `Welcome` (the first fix on the
    /// server's clock — real links run independent tick clocks).
    pub fn welcome_tick(&self) -> Option<u64> {
        self.welcome_tick
    }

    /// Client: translate outgoing input stamps into the SERVER's tick domain
    /// (local tick + offset). The driver sets it once on a real link (welcome
    /// tick + RTT + a lead margin − the local tick); the harness leaves it 0.
    pub fn set_input_stamp_offset(&mut self, offset: i64) {
        self.stamp_offset = offset;
    }

    /// The active stamp offset — subtract it from an authoritative state's
    /// tick to get back into the local (prediction-ring) tick domain.
    pub fn input_stamp_offset(&self) -> i64 {
        self.stamp_offset
    }

    /// Client: queue this tick's input for the server (sent with the last few
    /// ticks as a redundant window on the next [`Self::tick_client`]). `tick`
    /// is LOCAL; the stamp offset translates it to the server's clock.
    pub fn send_input(&mut self, tick: u64, input: NetInput) {
        let stamped = (tick as i64 + self.stamp_offset).max(0) as u64;
        self.input_window.push_back(InputCmd { tick: stamped, input });
        while self.input_window.len() > INPUT_WINDOW {
            self.input_window.pop_front();
        }
    }

    /// Server: the input to run `tick` with for `peer` — the exact command if
    /// it arrived, else a repeat of the last known input (late/lost packets
    /// must not freeze the character; the correction flows back as prediction
    /// error on the owner, which is the standard, honest tradeoff).
    pub fn input_for(&mut self, peer: PeerId, tick: u64) -> NetInput {
        let buf = self.peer_inputs.entry(peer).or_default();
        // Drop stale ticks; adopt an exact match if present.
        while buf.front().is_some_and(|c| c.tick < tick) {
            let old = buf.pop_front().unwrap();
            self.last_input.insert(peer, old.input);
        }
        if let Some(cmd) = buf.pop_front_if(|c| c.tick == tick) {
            self.last_input.insert(peer, cmd.input.clone());
            return cmd.input;
        }
        self.late_inputs += 1;
        self.last_input.get(&peer).cloned().unwrap_or_default()
    }

    /// Server diagnostics: how many tick-inputs missed their tick (repeat-last
    /// used). Should sit near zero with a healthy clock skew.
    pub fn late_inputs(&self) -> u64 {
        self.late_inputs
    }

    /// Client: authoritative states received for OUR OWN predicted node —
    /// (entity, server tick, state) — the driver's reconcile input.
    pub fn take_predicted_updates(&mut self) -> Vec<(Entity, u64, PredictedState)> {
        std::mem::take(&mut self.predicted_in)
    }

    /// Server: spawn a replicated runtime node — locally now, on every client
    /// via a reliable `Spawn`, and re-sent to late joiners.
    pub fn spawn_doc(
        &mut self,
        world: &mut World,
        node: &floptle_scene::NodeDoc,
        owner: Option<PeerId>,
    ) -> Entity {
        debug_assert_eq!(self.role, NetRole::Server, "only the server spawns");
        let e = floptle_scene::spawn_node(node, world);
        // Ensure the node replicates (a doc without the component still nets —
        // spawning through the session IS the intent to replicate).
        let mut rep = world.get::<Replicated>(e).copied().unwrap_or_default();
        rep.owner = owner;
        world.insert(e, rep);
        let id = self.next_id;
        self.next_id += 1;
        self.net_to_ent.insert(id, e);
        self.ent_to_net.insert(e, id);
        let ron = ron::to_string(node).unwrap_or_default();
        self.spawned_docs.insert(id, (ron.clone(), owner));
        let msg = Msg::Spawn { id, node_ron: ron, owner }.encode();
        for &p in &self.peers {
            self.transport.send(p, Channel::Reliable, &msg);
        }
        e
    }

    /// Server: despawn a replicated node everywhere.
    pub fn despawn(&mut self, world: &mut World, e: Entity) {
        debug_assert_eq!(self.role, NetRole::Server, "only the server despawns");
        let Some(id) = self.ent_to_net.remove(&e) else { return };
        self.net_to_ent.remove(&id);
        self.spawned_docs.remove(&id);
        self.last_sent.remove(&id);
        world.despawn(e);
        let msg = Msg::Despawn { id }.encode();
        for &p in &self.peers {
            self.transport.send(p, Channel::Reliable, &msg);
        }
    }

    // -----------------------------------------------------------------------
    // Server tick
    // -----------------------------------------------------------------------

    /// Server: poll + process incoming traffic (joins/leaves/RPCs/inputs).
    /// The prediction-era driver calls this at TICK START — before scripts run
    /// — so [`Self::input_for`] hands `fixedUpdate` this tick's freshest client
    /// inputs. [`Self::tick_server`] also calls it, so a simple driver that
    /// only ticks at the end still works.
    pub fn pump_server(&mut self, world: &World, tick: u64) {
        for inc in self.transport.poll() {
            match inc {
                Incoming::Connected(_) => { /* wait for Hello to admit */ }
                Incoming::Disconnected(p) => self.drop_peer(p),
                Incoming::Message(p, _, bytes) => {
                    let Some(msg) = Msg::decode(&bytes) else { continue };
                    self.server_message(world, p, msg, tick);
                }
            }
        }
    }

    /// Server, once per gameplay tick AFTER physics: handle joins/leaves/RPCs,
    /// then (at the snapshot cadence) send changed state.
    pub fn tick_server(&mut self, world: &World, tick: u64) {
        self.pump_server(world, tick);
        // Flush queued RPCs (server → clients; no perceived-tick stamp — the
        // server's view is the authority).
        for (target, name, args, _) in std::mem::take(&mut self.rpcs_out) {
            let msg = Msg::Rpc { name, args, sender: SERVER, tick: None }.encode();
            match target {
                RpcTarget::All => {
                    for &p in &self.peers {
                        self.transport.send(p, Channel::Reliable, &msg);
                    }
                }
                RpcTarget::Peer(p) => self.transport.send(p, Channel::Reliable, &msg),
                RpcTarget::Server => { /* server → server: loop back locally */
                    if let Some(Msg::Rpc { name, args, .. }) = Msg::decode(&msg) {
                        self.rpcs_in.push(ReceivedRpc { name, args, sender: SERVER, tick: None });
                    }
                }
            }
        }
        if self.peers.is_empty() || !tick.is_multiple_of(SNAPSHOT_EVERY as u64) {
            return;
        }
        self.snap_count += 1;
        let keyframe = self.snap_count % KEYFRAME_EVERY == 1;
        let snapshot = self.build_snapshot(world, tick, keyframe);
        if let Some(msg) = snapshot {
            let bytes = msg.encode();
            for &p in &self.peers {
                self.transport.send(p, Channel::UnreliableSequenced, &bytes);
            }
        }
    }

    fn server_message(&mut self, world: &World, from: PeerId, msg: Msg, tick: u64) {
        match msg {
            Msg::Hello { proto } => {
                if proto != PROTO_VERSION {
                    let refuse = Msg::Refused {
                        reason: format!("protocol {proto} != {PROTO_VERSION}"),
                    };
                    self.transport.send(from, Channel::Reliable, &refuse.encode());
                    return;
                }
                self.peers.push(from);
                self.events.push(NetEvent::PeerJoined(from));
                let welcome =
                    Msg::Welcome { peer: from, tick, snapshot_every: SNAPSHOT_EVERY };
                self.transport.send(from, Channel::Reliable, &welcome.encode());
                // Tell everyone else, and tell the joiner about existing peers.
                let joined = Msg::PeerJoined { peer: from }.encode();
                for &p in &self.peers {
                    if p != from {
                        self.transport.send(p, Channel::Reliable, &joined);
                        self.transport
                            .send(from, Channel::Reliable, &Msg::PeerJoined { peer: p }.encode());
                    }
                }
                // Late-join catch-up: runtime spawns, then a full keyframe.
                let spawns: Vec<Msg> = self
                    .spawned_docs
                    .iter()
                    .map(|(&id, (ron, owner))| Msg::Spawn {
                        id,
                        node_ron: ron.clone(),
                        owner: *owner,
                    })
                    .collect();
                for s in spawns {
                    self.transport.send(from, Channel::Reliable, &s.encode());
                }
                if let Some(kf) = self.build_full_snapshot(world, tick) {
                    // Reliable: the joiner MUST get its baseline.
                    self.transport.send(from, Channel::Reliable, &kf.encode());
                }
            }
            Msg::Rpc { name, args, tick: perceived, .. } => {
                // Stamp the true sender — never trust the payload's claim. The
                // perceived tick is clamped at rewind time, not here.
                self.rpcs_in.push(ReceivedRpc { name, args, sender: from, tick: perceived });
            }
            Msg::Input { entries } => {
                let buf = self.peer_inputs.entry(from).or_default();
                for cmd in entries {
                    // The window re-carries recent ticks; keep each tick once,
                    // in order (sequenced channel ⇒ arrivals are monotonic).
                    if buf.back().is_none_or(|last| cmd.tick > last.tick) {
                        buf.push_back(cmd);
                    }
                }
                while buf.len() > INPUT_BUFFER_CAP {
                    buf.pop_front();
                }
            }
            Msg::Bye => self.drop_peer(from),
            _ => { /* clients don't send anything else */ }
        }
    }

    fn drop_peer(&mut self, p: PeerId) {
        if let Some(i) = self.peers.iter().position(|&x| x == p) {
            self.peers.remove(i);
            self.events.push(NetEvent::PeerLeft(p));
            let left = Msg::PeerLeft { peer: p }.encode();
            for &q in &self.peers {
                self.transport.send(q, Channel::Reliable, &left);
            }
        }
    }

    /// Changed-only snapshot (or a keyframe: everything, healing lost sends).
    fn build_snapshot(&mut self, world: &World, tick: u64, keyframe: bool) -> Option<Msg> {
        let mut entries = Vec::new();
        for (e, rep) in world.query::<Replicated>() {
            if !rep.transform {
                continue;
            }
            let Some(&id) = self.ent_to_net.get(&e) else { continue };
            let Some(tr) = world.get::<Transform>(e) else { continue };
            let pos = [tr.translation.x, tr.translation.y, tr.translation.z];
            let rot = tr.rotation.to_array();
            let changed = self.last_sent.get(&id).is_none_or(|(p, r)| *p != pos || *r != rot);
            if keyframe || changed {
                self.last_sent.insert(id, (pos, rot));
                let body = rep.physics.then(|| self.body_states.get(&e).copied()).flatten();
                entries.push(SnapEntry {
                    id,
                    pos,
                    rot,
                    vel: body.map(|(v, _)| v),
                    grounded: body.map(|(_, g)| g),
                });
            }
        }
        let mut synced = Vec::new();
        for (e, script, vars) in &self.synced_now {
            let Some(&id) = self.ent_to_net.get(e) else { continue };
            let mut changed_vars = Vec::new();
            for (k, v) in vars {
                let key = (id, script.clone(), k.clone());
                if keyframe || self.last_synced.get(&key) != Some(v) {
                    self.last_synced.insert(key, v.clone());
                    changed_vars.push((k.clone(), v.clone()));
                }
            }
            if !changed_vars.is_empty() {
                synced.push(SyncedEntry { id, script: script.clone(), vars: changed_vars });
            }
        }
        if entries.is_empty() && synced.is_empty() && !keyframe {
            return None;
        }
        Some(Msg::Snapshot { tick, keyframe, entries, synced })
    }

    /// A full-state snapshot regardless of change detection (late-join baseline).
    fn build_full_snapshot(&mut self, world: &World, tick: u64) -> Option<Msg> {
        let mut entries = Vec::new();
        for (e, rep) in world.query::<Replicated>() {
            if !rep.transform {
                continue;
            }
            let (Some(&id), Some(tr)) = (self.ent_to_net.get(&e), world.get::<Transform>(e))
            else {
                continue;
            };
            let body = rep.physics.then(|| self.body_states.get(&e).copied()).flatten();
            entries.push(SnapEntry {
                id,
                pos: [tr.translation.x, tr.translation.y, tr.translation.z],
                rot: tr.rotation.to_array(),
                vel: body.map(|(v, _)| v),
                grounded: body.map(|(_, g)| g),
            });
        }
        let synced = self
            .synced_now
            .iter()
            .filter_map(|(e, script, vars)| {
                let &id = self.ent_to_net.get(e)?;
                Some(SyncedEntry { id, script: script.clone(), vars: vars.clone() })
            })
            .collect::<Vec<_>>();
        if entries.is_empty() && synced.is_empty() {
            return None;
        }
        Some(Msg::Snapshot { tick, keyframe: true, entries, synced })
    }

    // -----------------------------------------------------------------------
    // Client tick
    // -----------------------------------------------------------------------

    /// Client, once per gameplay tick: poll, buffer snapshots, apply the
    /// interpolated state a fixed delay behind the server.
    pub fn tick_client(&mut self, world: &mut World) {
        for inc in self.transport.poll() {
            match inc {
                Incoming::Message(_, _, bytes) => {
                    let Some(msg) = Msg::decode(&bytes) else { continue };
                    self.client_message(world, msg);
                }
                Incoming::Disconnected(_) => {
                    self.connected = false;
                    self.events.push(NetEvent::Disconnected("server closed".into()));
                }
                Incoming::Connected(_) => {}
            }
        }
        // Flush queued client → server RPCs (perceived-tick stamps ride along).
        for (_, name, args, stamp) in std::mem::take(&mut self.rpcs_out) {
            let msg = Msg::Rpc { name, args, sender: SERVER /* stamped by server */, tick: stamp };
            self.transport.send(SERVER, Channel::Reliable, &msg.encode());
        }
        // Ship the input window (this tick + the last few, redundantly).
        if self.connected && !self.input_window.is_empty() {
            let msg = Msg::Input { entries: self.input_window.iter().cloned().collect() };
            self.transport.send(SERVER, Channel::UnreliableSequenced, &msg.encode());
        }
        self.apply_interpolation(world);
    }

    fn client_message(&mut self, world: &mut World, msg: Msg) {
        match msg {
            Msg::Welcome { peer, tick, .. } => {
                self.connected = true;
                self.my_peer = Some(peer);
                self.welcome_tick = Some(tick);
                self.events.push(NetEvent::Connected);
            }
            Msg::Refused { reason } => {
                self.connected = false;
                self.events.push(NetEvent::Disconnected(reason));
            }
            Msg::Spawn { id, node_ron, owner } => {
                if self.net_to_ent.contains_key(&id) {
                    return; // duplicate catch-up
                }
                let Ok(node) = ron::from_str::<floptle_scene::NodeDoc>(&node_ron) else {
                    return;
                };
                let e = floptle_scene::spawn_node(&node, world);
                let mut rep = world.get::<Replicated>(e).copied().unwrap_or_default();
                rep.owner = owner;
                world.insert(e, rep);
                self.net_to_ent.insert(id, e);
                self.ent_to_net.insert(e, id);
                self.interp.insert(id, InterpBuf::new(&rep));
            }
            Msg::Despawn { id } => {
                if let Some(e) = self.net_to_ent.remove(&id) {
                    self.ent_to_net.remove(&e);
                    self.interp.remove(&id);
                    world.despawn(e);
                }
            }
            Msg::Snapshot { tick, entries, synced, .. } => {
                if tick <= self.latest_server_tick && tick != 0 && !entries.is_empty() {
                    // Sequenced channel already drops stale, but the reliable
                    // late-join keyframe can race a newer unreliable snapshot.
                    if tick < self.latest_server_tick {
                        return;
                    }
                }
                self.latest_server_tick = self.latest_server_tick.max(tick);
                for en in entries {
                    // OUR OWN predicted node never interpolates — its
                    // authoritative states go to the reconcile queue instead
                    // (docs/netcode-design.md §6).
                    if let Some(&e) = self.net_to_ent.get(&en.id)
                        && let Some(rep) = world.get::<Replicated>(e)
                        && rep.mode == floptle_core::ReplicationMode::Predicted
                        && rep.owner.is_some()
                        && rep.owner == self.my_peer
                    {
                        self.predicted_in.push((
                            e,
                            tick,
                            PredictedState {
                                pos: en.pos,
                                rot: en.rot,
                                vel: en.vel.unwrap_or([0.0; 3]),
                                grounded: en.grounded.unwrap_or(false),
                            },
                        ));
                        continue;
                    }
                    let buf = self
                        .interp
                        .entry(en.id)
                        .or_insert_with(|| InterpBuf::new(&Replicated::default()));
                    buf.samples.push_back((tick, en.pos, en.rot));
                    while buf.samples.len() > MAX_SAMPLES {
                        buf.samples.pop_front();
                    }
                }
                for s in synced {
                    if let Some(&e) = self.net_to_ent.get(&s.id) {
                        self.synced_in.push((e, s.script, s.vars));
                    }
                }
            }
            Msg::Rpc { name, args, sender, tick } => {
                self.rpcs_in.push(ReceivedRpc { name, args, sender, tick });
            }
            Msg::PeerJoined { peer } => self.events.push(NetEvent::PeerJoined(peer)),
            Msg::PeerLeft { peer } => self.events.push(NetEvent::PeerLeft(peer)),
            Msg::Bye => {
                self.connected = false;
                self.events.push(NetEvent::Disconnected("server said bye".into()));
            }
            _ => {}
        }
    }

    /// Write each replicated entity's transform at `latest - its interp delay`
    /// (per-node, from the Networked component), lerped between the two
    /// bracketing samples (`interp = false` snaps to the newest instead).
    fn apply_interpolation(&mut self, world: &mut World) {
        let latest = self.latest_server_tick;
        for (id, buf) in &mut self.interp {
            let target = latest.saturating_sub(buf.delay);
            let Some(&e) = self.net_to_ent.get(id) else { continue };
            let Some(last) = buf.samples.back().copied() else { continue };
            let (pos, rot) = if !buf.interp || buf.samples.len() == 1 {
                (last.1, last.2)
            } else {
                // Find the pair bracketing `target`.
                let mut a = *buf.samples.front().unwrap();
                let mut b = last;
                for w in buf.samples.iter().copied().collect::<Vec<_>>().windows(2) {
                    if w[0].0 <= target && target <= w[1].0 {
                        a = w[0];
                        b = w[1];
                        break;
                    }
                    if w[1].0 <= target {
                        a = w[1];
                        b = last;
                    }
                }
                if b.0 <= a.0 {
                    (b.1, b.2)
                } else {
                    let t = ((target.saturating_sub(a.0)) as f32 / (b.0 - a.0) as f32)
                        .clamp(0.0, 1.0);
                    let pa = DVec3::from_array(a.1);
                    let pb = DVec3::from_array(b.1);
                    let p = pa.lerp(pb, t as f64);
                    let qa = Quat::from_array(a.2);
                    let qb = Quat::from_array(b.2);
                    let q = qa.slerp(qb, t);
                    (p.to_array(), q.to_array())
                }
            };
            if let Some(tr) = world.get_mut::<Transform>(e) {
                tr.translation = DVec3::from_array(pos);
                tr.rotation = Quat::from_array(rot).normalize();
            }
            // Trim samples far behind the target so the buffer stays small.
            while buf.samples.len() > 2
                && buf.samples[1].0 < target.saturating_sub(buf.delay)
            {
                buf.samples.pop_front();
            }
        }
    }
}
