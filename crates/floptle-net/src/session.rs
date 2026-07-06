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

use crate::transport::{Channel, Incoming, LinkStats, PeerId, Transport, SERVER};
use crate::value::NetValue;
use crate::wire::{Msg, SnapEntry, SyncedEntry, PROTO_VERSION};

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
/// the receiving side's transport identity — a client can't spoof it.
#[derive(Clone, Debug)]
pub struct ReceivedRpc {
    pub name: String,
    pub args: NetValue,
    pub sender: PeerId,
}

/// Per-entity `synced` script vars: (entity, script kind, name→value pairs).
pub type SyncedVars = Vec<(Entity, String, Vec<(String, NetValue)>)>;

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
    // --- client ---
    connected: bool,
    interp: HashMap<u64, InterpBuf>,
    latest_server_tick: u64,
    // --- both ---
    events: Vec<NetEvent>,
    rpcs_in: Vec<ReceivedRpc>,
    rpcs_out: Vec<(RpcTarget, String, NetValue)>,
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
            connected: false,
            interp: HashMap::new(),
            latest_server_tick: 0,
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

    /// Queue an outgoing RPC. Guardrails apply: an over-limit value is dropped
    /// whole with an error string returned (surface it in the Console).
    pub fn send_rpc(
        &mut self,
        name: &str,
        args: NetValue,
        target: RpcTarget,
    ) -> Result<(), String> {
        args.validate().map_err(|e| format!("net.rpc(\"{name}\"): {e}"))?;
        self.rpcs_out.push((target, name.to_string(), args));
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

    /// Server, once per gameplay tick AFTER physics: handle joins/leaves/RPCs,
    /// then (at the snapshot cadence) send changed state.
    pub fn tick_server(&mut self, world: &World, tick: u64) {
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
        // Flush queued RPCs (server → clients).
        for (target, name, args) in std::mem::take(&mut self.rpcs_out) {
            let msg = Msg::Rpc { name, args, sender: SERVER }.encode();
            match target {
                RpcTarget::All => {
                    for &p in &self.peers {
                        self.transport.send(p, Channel::Reliable, &msg);
                    }
                }
                RpcTarget::Peer(p) => self.transport.send(p, Channel::Reliable, &msg),
                RpcTarget::Server => { /* server → server: loop back locally */
                    if let Some(Msg::Rpc { name, args, .. }) = Msg::decode(&msg) {
                        self.rpcs_in.push(ReceivedRpc { name, args, sender: SERVER });
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
            Msg::Rpc { name, args, .. } => {
                // Stamp the true sender — never trust the payload's claim.
                self.rpcs_in.push(ReceivedRpc { name, args, sender: from });
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
                entries.push(SnapEntry { id, pos, rot, vel: None });
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
            entries.push(SnapEntry {
                id,
                pos: [tr.translation.x, tr.translation.y, tr.translation.z],
                rot: tr.rotation.to_array(),
                vel: None,
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
        // Flush queued client → server RPCs.
        for (_, name, args) in std::mem::take(&mut self.rpcs_out) {
            let msg = Msg::Rpc { name, args, sender: SERVER /* stamped by server */ };
            self.transport.send(SERVER, Channel::Reliable, &msg.encode());
        }
        self.apply_interpolation(world);
    }

    fn client_message(&mut self, world: &mut World, msg: Msg) {
        match msg {
            Msg::Welcome { .. } => {
                self.connected = true;
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
            Msg::Rpc { name, args, sender } => {
                self.rpcs_in.push(ReceivedRpc { name, args, sender });
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
