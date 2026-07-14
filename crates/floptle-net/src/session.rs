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

use std::collections::{HashMap, HashSet, VecDeque};

use floptle_core::math::{DVec3, Quat};
use floptle_core::transform::Transform;
use floptle_core::{Entity, Replicated, World};

use crate::predict::PredictedState;
use crate::transport::{Channel, Incoming, LinkStats, PeerId, Transport, SERVER};
use crate::value::NetValue;
use crate::wire::{
    AnimEntry, AnimLayerWire, InputCmd, Msg, NetInput, SnapEntry, SyncedEntry, PROTO_VERSION,
};

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

/// One controller layer's live playback as fed by the driver (pre-quantization).
/// Mirrors `floptle_anim::NetLayerState` without coupling this crate to it.
/// `dur`/`looped`/`rate` never hit the wire — they power the SEND-side change
/// predictor: a playing clip's time is foreseeable (t + elapsed·rate, wrapped
/// on loops), so an undisturbed animation costs ZERO bytes after its
/// transition. Only surprises (transitions, seeks, speed/weight edits, drift)
/// are sent.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnimSrcLayer {
    pub state: Option<u16>,
    pub t: f32,
    pub weight: f32,
    pub dur: f32,
    pub looped: bool,
    pub rate: f32,
}

/// Per-entity animator states fed by the driver each tick:
/// (entity, controller-global speed, layers).
pub type AnimStates = Vec<(Entity, f32, Vec<AnimSrcLayer>)>;

/// What the server last sent for one animator — the change predictor's base.
struct AnimSent {
    tick: u64,
    speed: i16,
    layers: Vec<(AnimLayerWire, f32, bool, f32)>, // (+ dur, looped, rate)
}

/// Bandwidth guardrail: layers per animator on the wire (controllers with more
/// are pathological; the tail is silently untracked).
const MAX_ANIM_LAYERS: usize = 8;
/// Send-side time-prediction tolerance, seconds — beyond this the sender
/// re-syncs the layer's clock (a seek, a hitch, a rate the predictor missed).
const ANIM_TIME_TOLERANCE: f32 = 0.1;

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

/// Ticks between per-peer [`Msg::InputAck`] sends (4 Hz at the 60 Hz tick).
const INPUT_ACK_EVERY: u64 = 15;

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
    /// Current animator states, refreshed by the driver each tick (diffed here
    /// against `last_anim`'s time prediction).
    anims_now: AnimStates,
    /// Per-NetId last-sent animator state — the change predictor's base.
    last_anim: HashMap<u64, AnimSent>,
    /// Gameplay-tick length in seconds (the time predictor's clock); the
    /// driver sets it at session start. Default 60 Hz.
    tick_dt: f32,
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
    /// Per-peer input-buffer margin, EWMA over [`Self::input_for`] calls:
    /// newest buffered stamp − the tick being consumed. Shipped back in
    /// [`Msg::InputAck`] so the owner can auto-tune its lead.
    peer_margin: HashMap<PeerId, f32>,
    /// Per-peer repeat-last count (the per-peer breakdown of `late_inputs`).
    peer_late: HashMap<PeerId, u64>,
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
    /// Auto-tune the stamp offset from [`Msg::InputAck`] margins (real links
    /// only — the in-editor harness shares one clock and must stay at 0).
    auto_lead: bool,
    /// Recent (stamp, local tick) pairs from [`Self::send_input`] — the exact
    /// translation reconcile needs, valid even across auto-lead nudges (the
    /// arithmetic `− stamp_offset` is only right for the CURRENT offset).
    sent_stamps: VecDeque<(u64, u64)>,
    /// The local tick of the newest [`Self::send_input`] (auto-tune cooldown).
    last_local_tick: u64,
    /// Local tick of the last auto-lead nudge.
    last_lead_change: u64,
    /// Latest [`Msg::InputAck`] payload from the server, if any.
    ack: Option<(i32, u64)>,
    /// Auto-lead adjustments since the last drain: (new offset, margin seen).
    lead_events: Vec<(i64, i32)>,
    interp: HashMap<u64, InterpBuf>,
    latest_server_tick: u64,
    /// Outgoing input window (last few ticks, resent redundantly).
    input_window: VecDeque<InputCmd>,
    /// Authoritative states received for OUR OWN predicted node, for the
    /// driver's reconcile step: (entity, server tick, state).
    predicted_in: Vec<(Entity, u64, PredictedState)>,
    /// Replicated spawns materialized since the last drain — (NetId, entity,
    /// owner). The driver registers physics bodies / binds prediction.
    spawned_in: Vec<(u64, Entity, Option<PeerId>)>,
    /// Entities despawned since the last drain (their bodies must go too).
    despawned_in: Vec<u32>,
    /// Received animator entries per NetId, buffered as (server tick, local
    /// arrival tick, entry) so they apply on the SAME delayed timeline as the
    /// transforms they accompany — a jump animation lands with the jump arc,
    /// not `interp_delay` early. The local arrival tick bounds the wait when
    /// traffic is sparse (a still NPC emoting sends nothing else, so the
    /// server-tick clock stalls): an entry applies at most `delay` local
    /// ticks after it arrived.
    anim_bufs: HashMap<u64, VecDeque<(u64, u64, AnimEntry)>>,
    /// Local tick_client call counter (the sparse-traffic aging clock above).
    client_ticks: u64,
    /// NetIds whose first animator state was already applied (the first one
    /// skips the interp delay — a late joiner's baseline shows immediately).
    anim_started: HashSet<u64>,
    /// Animator updates due this tick, for the driver's apply step.
    anims_due: Vec<(Entity, AnimEntry)>,
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
            peer_margin: HashMap::new(),
            peer_late: HashMap::new(),
            connected: false,
            my_peer: None,
            welcome_tick: None,
            stamp_offset: 0,
            auto_lead: false,
            sent_stamps: VecDeque::new(),
            last_local_tick: 0,
            last_lead_change: 0,
            ack: None,
            lead_events: Vec::new(),
            interp: HashMap::new(),
            latest_server_tick: 0,
            input_window: VecDeque::new(),
            predicted_in: Vec::new(),
            spawned_in: Vec::new(),
            despawned_in: Vec::new(),
            anim_bufs: HashMap::new(),
            anim_started: HashSet::new(),
            anims_due: Vec::new(),
            client_ticks: 0,
            events: Vec::new(),
            rpcs_in: Vec::new(),
            rpcs_out: Vec::new(),
            synced_in: Vec::new(),
            anims_now: Vec::new(),
            last_anim: HashMap::new(),
            tick_dt: 1.0 / 60.0,
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

    /// Server: refresh live animator states (the driver reads each networked
    /// controller each tick; the session diffs against its time prediction and
    /// sends only surprises — see [`AnimSrcLayer`]).
    pub fn update_anim_states(&mut self, states: AnimStates) {
        self.anims_now = states;
    }

    /// The gameplay-tick length in seconds — the animator time predictor's
    /// clock. Set once at session start (defaults to 60 Hz).
    pub fn set_tick_dt(&mut self, dt: f32) {
        if dt > 0.0 {
            self.tick_dt = dt;
        }
    }

    /// Client: animator updates that came due this tick (per entity, already
    /// delayed onto the same timeline as interpolated transforms). The driver
    /// applies them to its animation system.
    pub fn take_anim_updates(&mut self) -> Vec<(Entity, AnimEntry)> {
        std::mem::take(&mut self.anims_due)
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

    /// Client: let the session retune its own input lead from the server's
    /// [`Msg::InputAck`] margins. Real links only — the in-editor harness
    /// shares one clock with its hidden server and must stay at offset 0.
    pub fn set_auto_input_lead(&mut self, on: bool) {
        self.auto_lead = on;
    }

    /// Client: the exact local tick whose input the server consumed at
    /// `stamp` (from the recent-sends map — see [`Self::send_input`]).
    /// Oldest match wins: after a −1 nudge two locals carry the same stamp
    /// and the server's monotonic ingest kept the FIRST.
    pub fn local_tick_for_stamp(&self, stamp: u64) -> Option<u64> {
        self.sent_stamps.iter().find(|(s, _)| *s == stamp).map(|(_, t)| *t)
    }

    /// Client: the latest server-reported input margin (ticks of runway our
    /// inputs have when consumed; negative = arriving late) and the server's
    /// repeat-last count for us. `None` until the first ack lands.
    pub fn input_ack(&self) -> Option<(i32, u64)> {
        self.ack
    }

    /// Auto-lead adjustments since the last drain: (ticks added to the lead —
    /// negative trims it, margin seen). The driver surfaces them (console).
    pub fn take_lead_events(&mut self) -> Vec<(i64, i32)> {
        std::mem::take(&mut self.lead_events)
    }

    /// One auto-lead step, if due: keep the server-side margin inside [1, 6].
    /// Too little runway → raise the lead fast (late inputs are misprediction
    /// storms); too much → shave one tick at a time (extra lead is only added
    /// latency). Cooldown one second so the server's EWMA can settle between
    /// nudges. A +N nudge skips N stamps (the server repeats-last once); a −1
    /// nudge duplicates one stamp (the server's monotonic ingest drops it) —
    /// both self-heal through the redundant input window.
    fn auto_tune_lead(&mut self) {
        let Some((margin, _)) = self.ack else { return };
        if !self.auto_lead || self.last_local_tick.saturating_sub(self.last_lead_change) < 60 {
            return;
        }
        let delta: i64 = match margin {
            m if m < 1 => i64::from(1 - m).min(10),
            m if m > 6 => -1,
            _ => 0,
        };
        if delta != 0 {
            self.stamp_offset += delta;
            self.last_lead_change = self.last_local_tick;
            self.lead_events.push((delta, margin));
        }
    }

    /// Client: queue this tick's input for the server (sent with the last few
    /// ticks as a redundant window on the next [`Self::tick_client`]). `tick`
    /// is LOCAL; the stamp offset translates it to the server's clock.
    pub fn send_input(&mut self, tick: u64, input: NetInput) {
        let stamped = (tick as i64 + self.stamp_offset).max(0) as u64;
        self.last_local_tick = tick;
        // Remember the exact stamp→local pairing: reconcile translates the
        // server's authoritative tick back through THIS map, so auto-lead
        // nudges (which change the offset mid-flight) can't skew it.
        self.sent_stamps.push_back((stamped, tick));
        while self.sent_stamps.len() > 128 {
            self.sent_stamps.pop_front();
        }
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
        // Timing margin BEFORE consuming: how many ticks of runway the newest
        // buffered stamp still has past this tick. Negative = this peer's
        // inputs run late. Smoothed (EWMA) and shipped back via `InputAck` so
        // the owner can retune its lead.
        let now = buf.back().map(|c| c.tick as i64 - tick as i64).unwrap_or(-1) as f32;
        let m = self.peer_margin.entry(peer).or_insert(now);
        *m += 0.1 * (now - *m);
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
        *self.peer_late.entry(peer).or_default() += 1;
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

    /// Client: replicated spawns materialized since the last drain — the
    /// driver registers physics bodies and (for a spawn it owns) binds
    /// prediction to it.
    pub fn take_spawned(&mut self) -> Vec<(u64, Entity, Option<PeerId>)> {
        std::mem::take(&mut self.spawned_in)
    }

    /// Client: entities despawned since the last drain (entity indices) — the
    /// driver removes their physics bodies.
    pub fn take_despawned(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.despawned_in)
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

    /// Server: the runtime spawns owned by `peer` — what a disconnect should
    /// clean up (their player left; their avatar goes with them).
    pub fn owned_runtime_spawns(&self, peer: PeerId) -> Vec<Entity> {
        self.spawned_docs
            .iter()
            .filter(|(_, (_, o))| *o == Some(peer))
            .filter_map(|(id, _)| self.net_to_ent.get(id).copied())
            .collect()
    }

    /// Server: despawn a replicated node everywhere.
    pub fn despawn(&mut self, world: &mut World, e: Entity) {
        debug_assert_eq!(self.role, NetRole::Server, "only the server despawns");
        let Some(id) = self.ent_to_net.remove(&e) else { return };
        self.net_to_ent.remove(&id);
        self.spawned_docs.remove(&id);
        self.last_sent.remove(&id);
        self.last_anim.remove(&id);
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
        // Input-timing feedback, a few times a second: each peer learns how
        // much runway its inputs have (auto-lead reads this client-side).
        if !self.peers.is_empty() && tick.is_multiple_of(INPUT_ACK_EVERY) {
            for i in 0..self.peers.len() {
                let p = self.peers[i];
                let margin = self.peer_margin.get(&p).map(|m| m.round() as i32).unwrap_or(0);
                let late = self.peer_late.get(&p).copied().unwrap_or(0);
                let ack = Msg::InputAck { margin, late }.encode();
                self.transport.send(p, Channel::UnreliableSequenced, &ack);
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
            self.peer_inputs.remove(&p);
            self.last_input.remove(&p);
            self.peer_margin.remove(&p);
            self.peer_late.remove(&p);
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
        let anims = self.collect_anim_entries(tick, keyframe);
        if entries.is_empty() && synced.is_empty() && anims.is_empty() && !keyframe {
            return None;
        }
        Some(Msg::Snapshot { tick, keyframe, entries, synced, anims })
    }

    /// Encode the animator states that need sending: everything on a keyframe,
    /// else only the SURPRISED ones — a changed state/weight/speed, or a clock
    /// the time predictor couldn't foresee (a seek, a hitch). An undisturbed
    /// looping animation costs zero bytes here.
    fn collect_anim_entries(&mut self, tick: u64, keyframe: bool) -> Vec<AnimEntry> {
        let mut out = Vec::new();
        for (e, speed, layers) in &self.anims_now {
            let Some(&id) = self.ent_to_net.get(e) else { continue };
            let speed_q = AnimEntry::quantize_speed(*speed);
            let wire: Vec<AnimLayerWire> = layers
                .iter()
                .take(MAX_ANIM_LAYERS)
                .map(|l| AnimLayerWire::quantize(l.state, l.t, l.weight))
                .collect();
            let dirty = match self.last_anim.get(&id) {
                None => true,
                Some(sent) => {
                    sent.speed != speed_q
                        || sent.layers.len() != wire.len()
                        || sent.layers.iter().zip(wire.iter().zip(layers.iter())).any(
                            |((sw, dur, looped, rate), (w, src))| {
                                if sw.state != w.state || sw.weight != w.weight {
                                    return true;
                                }
                                if w.state == AnimLayerWire::STOPPED {
                                    return false;
                                }
                                // Where should its clock be, from what we last
                                // sent? (Wrapped on loops, clamped one-shots.)
                                let elapsed =
                                    tick.saturating_sub(sent.tick) as f32 * self.tick_dt;
                                let mut pt = sw.t_secs() + elapsed * rate;
                                if *looped && *dur > 1e-6 {
                                    pt = pt.rem_euclid(*dur);
                                } else if *dur > 1e-6 {
                                    pt = pt.clamp(0.0, *dur);
                                }
                                let lin = (pt - src.t).abs();
                                let dist = if *looped && *dur > 1e-6 {
                                    lin.min(*dur - lin)
                                } else {
                                    lin
                                };
                                dist > ANIM_TIME_TOLERANCE
                            },
                        )
                }
            };
            if keyframe || dirty {
                self.last_anim.insert(
                    id,
                    AnimSent {
                        tick,
                        speed: speed_q,
                        layers: wire
                            .iter()
                            .zip(layers.iter())
                            .map(|(w, l)| (*w, l.dur, l.looped, l.rate))
                            .collect(),
                    },
                );
                out.push(AnimEntry { id, speed: speed_q, layers: wire });
            }
        }
        out
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
        let anims = self
            .anims_now
            .iter()
            .filter_map(|(e, speed, layers)| {
                let &id = self.ent_to_net.get(e)?;
                Some(AnimEntry {
                    id,
                    speed: AnimEntry::quantize_speed(*speed),
                    layers: layers
                        .iter()
                        .take(MAX_ANIM_LAYERS)
                        .map(|l| AnimLayerWire::quantize(l.state, l.t, l.weight))
                        .collect(),
                })
            })
            .collect::<Vec<_>>();
        if entries.is_empty() && synced.is_empty() && anims.is_empty() {
            return None;
        }
        Some(Msg::Snapshot { tick, keyframe: true, entries, synced, anims })
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
        // Retune the input lead from the server's latest margin feedback
        // (window entries are already stamped — a nudge takes effect on the
        // next `send_input`).
        self.auto_tune_lead();
        // Ship the input window (this tick + the last few, redundantly).
        if self.connected && !self.input_window.is_empty() {
            let msg = Msg::Input { entries: self.input_window.iter().cloned().collect() };
            self.transport.send(SERVER, Channel::UnreliableSequenced, &msg.encode());
        }
        self.apply_interpolation(world);
        self.client_ticks += 1;
        self.collect_due_anims();
    }

    /// Move buffered animator entries whose tick has come due (latest − that
    /// node's interp delay, OR `delay` local ticks after arrival — whichever
    /// first) into the drain list, newest-due winning — animator changes land
    /// on the SAME delayed timeline as the transforms around them, and never
    /// stall behind sparse traffic. A node's FIRST state applies immediately:
    /// a late joiner's baseline pose must not idle out the interp delay.
    fn collect_due_anims(&mut self) {
        let latest = self.latest_server_tick;
        let now = self.client_ticks;
        self.anim_bufs.retain(|id, buf| {
            let Some(&e) = self.net_to_ent.get(id) else {
                return false; // entity gone — drop the buffer
            };
            let delay = self
                .interp
                .get(id)
                .map(|b| b.delay)
                .unwrap_or(Replicated::DEFAULT_INTERP_DELAY as u64);
            let target = latest.saturating_sub(delay);
            let mut due = None;
            while buf
                .front()
                .is_some_and(|(t, arrived, _)| *t <= target || now.saturating_sub(*arrived) >= delay)
            {
                due = buf.pop_front().map(|(_, _, en)| en);
            }
            if due.is_none() && !self.anim_started.contains(id) && !buf.is_empty() {
                due = buf.pop_front().map(|(_, _, en)| en);
            }
            if let Some(en) = due {
                self.anim_started.insert(*id);
                self.anims_due.push((e, en));
            }
            true
        });
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
                self.spawned_in.push((id, e, owner));
            }
            Msg::Despawn { id } => {
                if let Some(e) = self.net_to_ent.remove(&id) {
                    self.ent_to_net.remove(&e);
                    self.interp.remove(&id);
                    self.anim_bufs.remove(&id);
                    self.anim_started.remove(&id);
                    self.despawned_in.push(e.index());
                    world.despawn(e);
                }
            }
            Msg::Snapshot { tick, entries, synced, anims, .. } => {
                if tick <= self.latest_server_tick && tick != 0 && !entries.is_empty() {
                    // Sequenced channel already drops stale, but the reliable
                    // late-join keyframe can race a newer unreliable snapshot.
                    if tick < self.latest_server_tick {
                        return;
                    }
                }
                self.latest_server_tick = self.latest_server_tick.max(tick);
                for an in anims {
                    // OUR OWN predicted node's animator is locally driven (its
                    // scripts run here, ahead of the server) — never overwrite.
                    if let Some(&e) = self.net_to_ent.get(&an.id)
                        && let Some(rep) = world.get::<Replicated>(e)
                        && rep.mode == floptle_core::ReplicationMode::Predicted
                        && rep.owner.is_some()
                        && rep.owner == self.my_peer
                    {
                        continue;
                    }
                    let buf = self.anim_bufs.entry(an.id).or_default();
                    buf.push_back((tick, self.client_ticks, an));
                    while buf.len() > MAX_SAMPLES {
                        buf.pop_front();
                    }
                }
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
            Msg::InputAck { margin, late } => self.ack = Some((margin, late)),
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
