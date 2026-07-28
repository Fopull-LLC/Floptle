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

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

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
pub enum JoinState {
    /// Sent, nothing back yet. A relay round trip lives here.
    Connecting,
    /// In. The session is real.
    Joined,
    /// It will never succeed, and this is why — usually a code that matches
    /// no lobby.
    ///
    /// The distinction this exists to draw: `net.join` does not block and
    /// `net.role()` reads "client" from the frame it is called, so a game
    /// that trusts role congratulates a player on joining a lobby that was
    /// never there. Waiting on this separates "not yet" from "never",
    /// which elapsed time cannot.
    Refused(String),
}

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

/// Per-animator states fed by the driver each tick: (networked entity, sub
/// index — which animator under that node, see `wire::AnimEntry::sub` —
/// controller-global speed, layers).
pub type AnimStates = Vec<(Entity, u8, f32, Vec<AnimSrcLayer>)>;

/// One buffered inbound animator entry: (server tick, local arrival tick, entry).
type AnimBufEntry = (u64, u64, AnimEntry);

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
/// How many of a peer's unconfirmed applied ticks ride in one rollback fan-out.
///
/// Per PEER, so nobody can crowd anybody out, and generous next to what a
/// healthy session needs: a peer stalls once it is `max_depth` (8) past the
/// confirmed frontier, so in an ordinary match every ring holds well under this
/// and the cap never binds. It binds only when a peer has stopped confirming
/// entirely — where the oldest ticks are the ones that matter and the newest
/// are speculation nobody can use yet, which is why the fan-out takes from the
/// FRONT.
const FANOUT_PER_PEER: usize = 24;

/// What one snapshot entry costs a client's byte budget: a NetId, a position,
/// a rotation. Approximate on purpose — the budget is a rationing policy, not
/// an accounting ledger, and encoding every candidate to measure it exactly
/// would cost more than the rationing saves.
const SNAP_ENTRY_BYTES: usize = 8 + 3 * 8 + 4 * 4;
/// A transform baseline: the position and rotation an audience was last told.
type Pose = ([f64; 3], [f32; 4]);
/// The extra a physics-synced entry carries: velocity plus a grounded flag.
const SNAP_BODY_BYTES: usize = 3 * 4 + 1;

/// One replicable node, gathered once per snapshot round and then reused for
/// every peer — walking the WORLD per peer would put back exactly the
/// `players × entities` cost interest management exists to remove.
struct Replicable {
    id: u64,
    entity: Entity,
    rep: Replicated,
    pos: [f64; 3],
    rot: [f32; 4],
}

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
    /// Last transform sent per (audience, NetId) — the delta baseline.
    /// `None` is the BROADCAST audience: one baseline shared by every client,
    /// which is exactly right when every client gets the same snapshot.
    /// `Some(peer)` is that client's own, which interest management needs
    /// because no two clients are told the same thing any more.
    last_sent: HashMap<(Option<PeerId>, u64), Pose>,
    last_synced: HashMap<(Option<PeerId>, u64, String, String), NetValue>,
    /// Client: how the join attempt is going. `net.joinState()` reads it, so a
    /// lobby screen can say "no such lobby" instead of counting to ten.
    join_state: JoinState,
    /// Interest management (`docs/netcode-design.md` §5.2). Off by default.
    interest: crate::interest::InterestConfig,
    /// Per-client relevant sets and priority accumulators. Empty and unused
    /// while `interest.enabled` is false.
    interest_sets: crate::interest::InterestSets,
    /// What the last snapshot cost each client — read by the 🌐 panel so the
    /// feature is visible while it runs. Also empty while interest is off.
    interest_stats: HashMap<PeerId, crate::interest::InterestStat>,
    /// Current synced values, refreshed by the driver each tick (diffed here).
    synced_now: SyncedVars,
    /// Runtime spawns alive right now, for late-joiner catch-up.
    spawned_docs: HashMap<u64, (String, Option<PeerId>)>,
    snap_count: u32,
    /// The scene GENERATION: bumped by every [`Self::switch_scene`], carried on
    /// scene-scoped messages (snapshots/spawns/despawns) so old-scene state
    /// still in flight can't apply to the new scene's same-numbered NetIds.
    /// Clients adopt it from `Welcome` / `Scene`.
    scene_epoch: u8,
    /// The running scene, as a project-root-relative path — what `Welcome`
    /// tells late joiners. Set by the driver ([`Self::set_scene`]).
    scene: String,
    /// Client: a scene the server told us to be in (from `Scene` or a
    /// `Welcome` naming one), awaiting the driver's load + rebind.
    scene_switch_in: Option<String>,
    /// Client: a switch is announced but [`Self::rebind_scene`] hasn't run —
    /// scene-scoped messages are dropped until the driver rebinds (the old
    /// id map must never eat the new scene's snapshots).
    scene_pending: bool,
    /// Live body states fed by the driver each tick (velocity + grounded per
    /// physics-synced entity) — carried in snapshots for prediction.
    body_states: HashMap<Entity, ([f32; 3], bool)>,
    /// Current animator states, refreshed by the driver each tick (diffed here
    /// against `last_anim`'s time prediction).
    anims_now: AnimStates,
    /// Per-(audience, NetId, sub) last-sent animator state — the change
    /// predictor's base. Audience as for [`Self::last_sent`].
    last_anim: HashMap<(Option<PeerId>, u64, u8), AnimSent>,
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
    /// This peer's action-map fingerprint, compared at hello time. Zero means
    /// "no map" — a project that hasn't defined actions yet, which is fine as
    /// long as both sides agree.
    input_map_hash: u64,
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
    anim_bufs: HashMap<(u64, u8), VecDeque<AnimBufEntry>>,
    /// Local tick_client call counter (the sparse-traffic aging clock above).
    client_ticks: u64,
    /// (NetId, sub)s whose first animator state was already applied (the first
    /// skips the interp delay — a late joiner's baseline shows immediately).
    anim_started: HashSet<(u64, u8)>,
    /// Animator updates due this tick, for the driver's apply step.
    anims_due: Vec<(Entity, AnimEntry)>,
    // --- rollback (docs/rollback-netcode-design.md §5) ---
    /// Is this session simulating by rollback? Set by the driver from the
    /// scene's `Rollback` nodes. It changes what the host does with inputs
    /// (fan them out to everyone, rather than consume them itself) and turns
    /// auto input lead off, because the fixed delay replaces it.
    rollback: bool,
    /// The session's fixed input delay in ticks (§2.2). Host-set, carried in
    /// `Welcome` + `RollbackStart`, identical on every peer — mismatched delay
    /// is mismatched simulation.
    input_delay: u8,
    /// Peer→slot assignment, index = slot. `slots[0]` is always the host.
    rollback_slots: Vec<PeerId>,
    /// The match's RNG seed (§3) — host-chosen, carried in `RollbackStart`,
    /// identical on every peer. What `net.random()` draws from.
    rollback_seed: u64,
    /// Every peer's recent applied-tick inputs, **one ring per peer** — the
    /// redundant window the host echoes to everyone each tick. On the host this
    /// IS the session input log, which is what makes replays, the referee and
    /// (later) spectators nearly free (§5).
    ///
    /// Per peer, not one shared FIFO, and the distinction is load-bearing.
    /// Shared, a peer that resends its window every tick (which every peer
    /// does, that being the whole loss strategy) evicts OTHER peers' oldest
    /// entries — and the oldest entry is exactly the tick a starved peer is
    /// waiting for. That deadlocked a live match permanently off one lost
    /// datagram (floptle/0039). Per-peer rings make crowding impossible.
    ///
    /// `BTreeMap`, not `HashMap`: the fan-out is built by iterating this, and a
    /// packet whose contents depend on hash order is a packet that differs
    /// between two runs of the same match.
    rollback_rings: BTreeMap<PeerId, VecDeque<InputCmd>>,
    /// What each peer has told us its rollback frontier is — the newest applied
    /// tick for which THEY hold every peer's real input (`Msg::Input`'s
    /// `confirmed`; our own entry is set locally by the driver).
    ///
    /// The retention floor comes from the MINIMUM of these, never from our own
    /// frontier alone. "I have everyone's input for T" does not imply "everyone
    /// has everyone's input for T", and dropping on the former is the bug this
    /// field exists to make impossible.
    peer_confirmed: HashMap<PeerId, u64>,
    /// Our own driver's frontier, as last reported by
    /// [`Self::set_rollback_confirmed`] — what a client ships to the host.
    rollback_confirmed: u64,
    /// Applied-tick inputs received for OTHER peers — from a client on the
    /// host, from the host's fan-out on a client. The driver drains these into
    /// `Rollback::add_remote`.
    rollback_in: Vec<(PeerId, u64, NetInput)>,
    /// Client: a `RollbackStart` received since the last drain — the cue to
    /// (re)start the local driver at tick 0.
    rollback_start_in: Option<(Vec<PeerId>, u8, u64)>,
    /// Host: the match input log, when recording. Inputs plus the seed ARE the
    /// replay (`crate::replay`) — and the same log is what the referee
    /// re-simulates from.
    record: Option<crate::replay::InputLog>,
    /// Round-trip probes in flight: `(id, peer)` ⏵ when it was sent. Cleared
    /// on reply and aged out, so a peer that stops answering doesn't leak one
    /// entry per probe.
    pings_out: HashMap<(u32, PeerId), std::time::Instant>,
    /// Next probe id.
    ping_id: u32,
    /// Smoothed application-level round trip per peer, milliseconds. This is
    /// the honest number through a relay, where the transport can only see its
    /// own leg.
    peer_rtt: HashMap<PeerId, f32>,
    /// Host: newly recorded log entries, drained by whoever is shadowing the
    /// match (the referee). Handed out as they arrive rather than by cloning
    /// the log, which would cost the whole match every tick.
    record_out: Vec<crate::replay::LogEntry>,
    /// Host: the REFEREE's checksum per confirmed tick — from a simulation that
    /// never guessed. When one exists, peers are judged against it rather than
    /// against each other, which is the difference between "someone is wrong"
    /// and "peer 3 is wrong".
    referee_hashes: HashMap<u64, u64>,
    /// Host: `(tick, peer)` where a peer's state disagreed with the referee.
    referee_faults: Vec<(u64, PeerId)>,
    /// Host: reported checksums per confirmed tick, `(peer, hash)` (§6).
    state_hashes: HashMap<u64, Vec<(PeerId, u64)>>,
    /// Ticks a desync was detected or announced for, for the driver to surface.
    desyncs_in: Vec<u64>,
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
    /// `input_map_hash` is this peer's `floptle_input::InputMap::hash()`.
    /// Input commands index actions by position in that map, so a joiner whose
    /// map has a different SHAPE is refused rather than allowed to desync.
    pub fn server(transport: Box<dyn Transport>, input_map_hash: u64) -> Self {
        let mut s = Self::new(NetRole::Server, transport);
        s.input_map_hash = input_map_hash;
        s
    }

    /// Client: says hello immediately; `Connected` arrives via events once the
    /// server welcomes us.
    pub fn client(mut transport: Box<dyn Transport>, input_map_hash: u64) -> Self {
        let hello = Msg::Hello { proto: PROTO_VERSION, input_map: input_map_hash };
        transport.send(SERVER, Channel::Reliable, &hello.encode());
        let mut s = Self::new(NetRole::Client, transport);
        s.input_map_hash = input_map_hash;
        s
    }

    fn new(role: NetRole, transport: Box<dyn Transport>) -> Self {
        Self {
            role,
            transport,
            net_to_ent: HashMap::new(),
            ent_to_net: HashMap::new(),
            input_map_hash: 0,
            next_id: 1,
            peers: Vec::new(),
            last_sent: HashMap::new(),
            last_synced: HashMap::new(),
            join_state: JoinState::Connecting,
            interest: crate::interest::InterestConfig::default(),
            interest_sets: crate::interest::InterestSets::default(),
            interest_stats: HashMap::new(),
            synced_now: Vec::new(),
            spawned_docs: HashMap::new(),
            snap_count: 0,
            scene_epoch: 0,
            scene: String::new(),
            scene_switch_in: None,
            scene_pending: false,
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
            rollback: false,
            input_delay: crate::rollback::DEFAULT_INPUT_DELAY,
            rollback_slots: Vec::new(),
            rollback_seed: 0,
            rollback_rings: BTreeMap::new(),
            peer_confirmed: HashMap::new(),
            rollback_confirmed: 0,
            rollback_in: Vec::new(),
            rollback_start_in: None,
            pings_out: HashMap::new(),
            ping_id: 0,
            peer_rtt: HashMap::new(),
            record: None,
            record_out: Vec::new(),
            referee_hashes: HashMap::new(),
            referee_faults: Vec::new(),
            state_hashes: HashMap::new(),
            desyncs_in: Vec::new(),
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

    /// Driver, at session start: the scene the session is running, as a
    /// project-root-relative path — what `Welcome` tells late joiners to load.
    pub fn set_scene(&mut self, scene: &str) {
        self.scene = scene.to_string();
    }

    /// The current scene generation (tests / diagnostics).
    pub fn scene_epoch(&self) -> u8 {
        self.scene_epoch
    }

    /// Server: the session is switching scenes. Bumps the epoch and announces
    /// the new scene to every client (reliable). The driver loads the scene
    /// into its own world, then calls [`Self::rebind_scene`].
    pub fn switch_scene(&mut self, scene: &str) {
        debug_assert_eq!(self.role, NetRole::Server, "only the server switches scenes");
        self.scene = scene.to_string();
        self.scene_epoch = self.scene_epoch.wrapping_add(1);
        let msg = Msg::Scene { epoch: self.scene_epoch, scene: self.scene.clone() }.encode();
        for &p in &self.peers {
            self.transport.send(p, Channel::Reliable, &msg);
        }
    }

    /// Both roles, right after the driver loaded the (new) scene into `world`:
    /// drop every id binding and scene-scoped buffer from the old scene, then
    /// assign fresh deterministic NetIds against the new one. Peer links,
    /// input timing, and pending events survive — only scene-scoped state
    /// resets. The next server snapshot is a keyframe (the new baseline).
    pub fn rebind_scene(&mut self, world: &World) {
        self.net_to_ent.clear();
        self.ent_to_net.clear();
        self.next_id = 1;
        // Server baselines.
        self.last_sent.clear();
        self.last_synced.clear();
        self.interest_sets.clear();
        self.spawned_docs.clear();
        self.body_states.clear();
        self.anims_now.clear();
        self.last_anim.clear();
        self.snap_count = 0;
        // Client buffers.
        self.interp.clear();
        self.anim_bufs.clear();
        self.anim_started.clear();
        self.anims_due.clear();
        self.predicted_in.clear();
        self.spawned_in.clear();
        self.despawned_in.clear();
        self.synced_in.clear();
        self.scene_pending = false;
        self.register_scene(world);
    }

    /// Client: a scene the server told us to be in (a mid-session `Scene`
    /// switch, or the `Welcome` naming the session's current scene), drained
    /// once. The driver loads it locally (if it isn't already the running
    /// scene), then MUST call [`Self::rebind_scene`] — scene-scoped messages
    /// stay dropped until it does.
    pub fn take_scene_switch(&mut self) -> Option<String> {
        self.scene_switch_in.take()
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

    // -----------------------------------------------------------------------
    // Rollback (docs/rollback-netcode-design.md §5)
    // -----------------------------------------------------------------------

    /// Host: put this session into (or out of) rollback mode with a fixed input
    /// delay, and announce the peer→slot roster to every client.
    ///
    /// Announcing is also the **tick origin**: every peer starts its rollback
    /// clock at 0 on receiving it, so a bare applied-tick number means the same
    /// instant everywhere and no stamp translation is needed. Calling it again
    /// (a peer joined or left) restarts the match clock, which is why v1 does
    /// not support joining a rollback match in progress.
    /// Server: turn interest management on or off, and configure it
    /// (`docs/netcode-design.md` §5.2). Off is the default — below a few dozen
    /// players broadcasting is cheaper, and a feature that changes what reaches
    /// the wire should be one a project asks for.
    ///
    /// Switching modes drops every delta baseline: the two paths keep different
    /// ones (shared vs per-client), and carrying one across would leave clients
    /// diffing against state nobody sent them. The next snapshot is a keyframe
    /// by construction, because nothing is baselined.
    pub fn set_interest(&mut self, cfg: crate::interest::InterestConfig) {
        if cfg == self.interest {
            return;
        }
        self.interest = cfg;
        self.interest_sets.clear();
        self.interest_stats.clear();
        self.last_sent.clear();
        self.last_synced.clear();
        self.last_anim.clear();
        self.snap_count = 0;
    }

    /// The interest settings in force.
    pub fn interest(&self) -> crate::interest::InterestConfig {
        self.interest
    }

    /// What the last snapshot cost each connected client, in join order.
    ///
    /// Empty while interest management is off, which is the honest answer:
    /// with it off every client is told about everything, so there is no set
    /// to report and nothing being held back.
    /// Client: how the join attempt is going.
    ///
    /// Worth preferring over [`Self::role`] on a lobby screen: joining does not
    /// block, so role reads `Client` from the frame `net.join` was called,
    /// whether or not that lobby exists.
    pub fn join_state(&self) -> &JoinState {
        &self.join_state
    }

    pub fn interest_stats(&self) -> Vec<(PeerId, crate::interest::InterestStat)> {
        self.peers
            .iter()
            .filter_map(|p| self.interest_stats.get(p).map(|s| (*p, *s)))
            .collect()
    }

    pub fn set_rollback(&mut self, on: bool, input_delay: u8, seed: u64) {
        self.rollback = on;
        self.rollback_seed = seed;
        self.input_delay = input_delay.min(crate::rollback::MAX_DELAY);
        self.rollback_rings.clear();
        self.rollback_in.clear();
        // A new match is a new tick origin: every frontier restarts at 0, or the
        // retention floor would inherit the LAST match's numbers and immediately
        // discard the new match's opening ticks.
        self.peer_confirmed.clear();
        self.rollback_confirmed = 0;
        self.state_hashes.clear();
        if self.role != NetRole::Server {
            return;
        }
        self.rollback_slots = if on {
            std::iter::once(SERVER).chain(self.peers.iter().copied()).collect()
        } else {
            Vec::new()
        };
        if on {
            let msg = Msg::RollbackStart {
                peers: self.rollback_slots.clone(),
                input_delay: self.input_delay,
                seed: self.rollback_seed,
            }
            .encode();
            for &p in &self.peers {
                self.transport.send(p, Channel::Reliable, &msg);
            }
        }
    }

    pub fn is_rollback(&self) -> bool {
        self.rollback
    }

    /// The session's fixed input delay in ticks.
    pub fn input_delay(&self) -> u8 {
        self.input_delay
    }

    /// The peer→slot roster (index = slot; the host is slot 0).
    pub fn rollback_slots(&self) -> &[PeerId] {
        &self.rollback_slots
    }

    /// The match's RNG seed (§3).
    pub fn rollback_seed(&self) -> u64 {
        self.rollback_seed
    }

    /// Client: this tick's local input for its APPLIED tick.
    ///
    /// Deliberately not [`Self::send_input`]: that stamps through
    /// `stamp_offset`, the adaptive lead the `Predicted` path uses to keep
    /// inputs arriving just ahead of the server. In a rollback session the
    /// fixed delay IS the lead and the tick origin is shared, so applying an
    /// offset on top would shift one peer's inputs relative to everyone else's
    /// — the two mechanisms fight, and the delay loses.
    pub fn send_rollback_input(&mut self, applied: u64, input: NetInput) {
        self.input_window.push_back(InputCmd { tick: applied, input });
        while self.input_window.len() > INPUT_WINDOW {
            self.input_window.pop_front();
        }
    }

    /// Host: record its OWN local input into the log it fans out. The host is a
    /// player too, and its inputs reach the clients by exactly the same path as
    /// everyone else's.
    pub fn push_rollback_input(&mut self, applied: u64, input: NetInput) {
        self.note_rollback_input(SERVER, InputCmd { tick: applied, input });
    }

    /// Start recording this match (host only). Inputs are the replay file;
    /// nothing else needs capturing, because a rollback match is a pure
    /// function of them plus the seed.
    pub fn start_recording(&mut self, scene: &str) {
        if self.role != NetRole::Server {
            return;
        }
        let mut log = crate::replay::InputLog::new(
            scene,
            self.rollback_seed,
            self.input_delay,
            self.rollback_slots.clone(),
            self.input_map_hash,
        );
        // Everything already banked this match — the warm-up ticks land before
        // anyone thinks to press record.
        for (peer, ring) in &self.rollback_rings {
            for cmd in ring {
                log.record(*peer, cmd.tick, &cmd.input);
            }
        }
        self.record = Some(log);
    }

    /// A probe came back: fold its round trip into the smoothed estimate.
    fn note_pong(&mut self, id: u32, from: PeerId) {
        let Some(sent) = self.pings_out.remove(&(id, from)) else {
            return; // aged out, or never ours
        };
        let ms = sent.elapsed().as_secs_f32() * 1000.0;
        // Smoothed, because one probe is a sample of a jittery link and a
        // number that jumps is one nobody can act on. Weighted towards the
        // history at 0.7/0.3 — roughly a two-second memory at this cadence.
        let e = self.peer_rtt.entry(from).or_insert(ms);
        *e = *e * 0.7 + ms * 0.3;
    }

    /// Drop probes nobody answered, so a silent peer costs one entry rather
    /// than one per probe forever.
    fn expire_pings(&mut self, now: std::time::Instant) {
        self.pings_out
            .retain(|_, sent| now.duration_since(*sent) < std::time::Duration::from_secs(5));
    }

    /// Measured round trip to a peer in milliseconds, application level.
    ///
    /// Prefer this to `Transport::stats().rtt_ms` whenever the answer matters:
    /// through a relay the transport can only see its own leg, so it reports
    /// host↔relay and calls it the player's ping. This is host↔player, and it
    /// is measured the same way over every transport.
    pub fn peer_rtt_ms(&self, peer: PeerId) -> Option<f32> {
        self.peer_rtt.get(&peer).copied()
    }

    /// Every peer's measured round trip, for the panel.
    pub fn peer_rtts(&self) -> Vec<(PeerId, f32)> {
        let mut v: Vec<(PeerId, f32)> = self.peer_rtt.iter().map(|(p, r)| (*p, *r)).collect();
        v.sort_by_key(|(p, _)| *p);
        v
    }

    /// Log entries recorded since the last drain — what a shadow simulation
    /// feeds on.
    pub fn take_log_entries(&mut self) -> Vec<crate::replay::LogEntry> {
        std::mem::take(&mut self.record_out)
    }

    /// The referee's verdict for a confirmed tick: the state a simulation that
    /// never guessed arrived at. Peers are judged against this once it exists.
    pub fn set_referee_hash(&mut self, tick: u64, hash: u64) {
        if self.role != NetRole::Server {
            return;
        }
        self.referee_hashes.insert(tick, hash);
        // Judge anything that already reported for this tick and was waiting.
        let reported: Vec<(PeerId, u64)> =
            self.state_hashes.get(&tick).cloned().unwrap_or_default();
        for (p, h) in reported {
            if h != hash {
                self.referee_faults.push((tick, p));
            }
        }
        // A long match must not accumulate one entry per checksum forever.
        let floor = tick.saturating_sub(600);
        self.referee_hashes.retain(|t, _| *t >= floor);
    }

    /// `(tick, peer)` pairs where a peer's reported state disagreed with the
    /// referee's. Empty unless a referee is running.
    pub fn take_referee_faults(&mut self) -> Vec<(u64, PeerId)> {
        std::mem::take(&mut self.referee_faults)
    }

    /// The match log so far, if recording.
    pub fn recording(&self) -> Option<&crate::replay::InputLog> {
        self.record.as_ref()
    }

    /// Stop recording and take the log.
    pub fn take_recording(&mut self) -> Option<crate::replay::InputLog> {
        self.record.take()
    }

    fn note_rollback_input(&mut self, peer: PeerId, cmd: InputCmd) {
        // The redundant window re-carries recent ticks in every packet; the
        // duplicate is free (the driver's `add_remote` ignores it) but the log
        // must not grow one entry per resend. Deduped against THIS peer's ring
        // — a shared one made the answer depend on how chatty everybody else
        // had been, so an entry could age out of the dedup and be re-admitted
        // as though it were new.
        let ring = self.rollback_rings.entry(peer).or_default();
        if ring.iter().any(|c| c.tick == cmd.tick) {
            return;
        }
        // Sorted by tick: arrivals are near-ordered but not guaranteed ordered
        // (the window re-carries, and a resend can overtake), and the fan-out
        // sends oldest first. A ring that is only nearly sorted would put the
        // urgent tick in the middle of the packet instead of at its head.
        let at = ring.iter().position(|c| c.tick > cmd.tick).unwrap_or(ring.len());
        ring.insert(at, cmd.clone());
        self.rollback_in.push((peer, cmd.tick, cmd.input.clone()));
        // Recorded HERE because this is the one funnel every peer's input
        // passes through exactly once, the host's own included — recording at
        // the send sites instead would miss whatever path was added last.
        if let Some(log) = self.record.as_mut()
            && log.record(peer, cmd.tick, &cmd.input)
        {
            self.record_out.push(crate::replay::LogEntry {
                tick: cmd.tick,
                peer,
                input: cmd.input.clone(),
            });
        }
        self.trim_rollback_rings();
    }

    /// The oldest applied tick still worth carrying: one past the newest tick
    /// EVERY peer has confirmed. Below it, everyone already has everything.
    ///
    /// Unknown frontiers count as 0, so a peer that has not reported yet holds
    /// the floor down rather than letting the session drop what it needs.
    fn rollback_retain_floor(&self) -> u64 {
        // A client's rings are dedup memory, not a fan-out source: it is the
        // only consumer of what it holds, so its own frontier is the whole
        // answer. Asking the roster instead would pin the floor at 0 forever
        // (nobody reports theirs to a client) and the rings would only ever be
        // bounded by the hard cap.
        if self.role != NetRole::Server {
            return self.rollback_confirmed;
        }
        self.rollback_rings
            .keys()
            .chain(self.rollback_slots.iter())
            .map(|p| self.peer_confirmed.get(p).copied().unwrap_or(0))
            .min()
            .unwrap_or(0)
    }

    /// Drop what every peer has confirmed, keeping a little history for
    /// ordinary redundancy — and hard-cap each ring so a peer that never
    /// confirms costs bounded memory instead of unbounded.
    fn trim_rollback_rings(&mut self) {
        let floor = self.rollback_retain_floor().saturating_sub(INPUT_WINDOW as u64);
        for ring in self.rollback_rings.values_mut() {
            while ring.front().is_some_and(|c| c.tick <= floor) {
                ring.pop_front();
            }
            // The backstop. A wedged peer means the match is already over one
            // way or another; it must not also mean unbounded growth.
            while ring.len() > crate::rollback::INPUT_RING {
                ring.pop_front();
            }
        }
    }

    /// The fan-out payload: every peer's unconfirmed applied ticks, oldest
    /// first, capped per peer.
    ///
    /// Oldest first is the whole point. A starved peer is waiting on the OLDEST
    /// tick it is missing, so that tick has to be in the packet — and it has to
    /// stay in the packet on every resend until it is confirmed, which is what
    /// the floor guarantees. Capping per peer (rather than in total) is what
    /// stops one chatty or wedged peer from squeezing everyone else out.
    fn rollback_fanout(&self) -> Vec<(PeerId, InputCmd)> {
        let floor = self.rollback_retain_floor();
        let mut out = Vec::new();
        for (&peer, ring) in &self.rollback_rings {
            out.extend(
                ring.iter()
                    .filter(|c| c.tick > floor)
                    .take(FANOUT_PER_PEER)
                    .map(|c| (peer, c.clone())),
            );
        }
        out
    }

    /// Record a peer's reported rollback frontier, ignoring a value that would
    /// move it backwards (a stale resend must not un-confirm anything).
    fn note_peer_confirmed(&mut self, peer: PeerId, confirmed: u64) {
        let e = self.peer_confirmed.entry(peer).or_insert(0);
        if confirmed > *e {
            *e = confirmed;
            self.trim_rollback_rings();
        }
    }

    /// The local driver's rollback frontier, for retention and for the fan-out.
    ///
    /// A client also ships it to the host on the next `Msg::Input`; the host is
    /// a peer like any other and records its own here.
    pub fn set_rollback_confirmed(&mut self, confirmed: u64) {
        let me = if self.role == NetRole::Server { SERVER } else { self.my_peer.unwrap_or(SERVER) };
        self.rollback_confirmed = confirmed;
        self.note_peer_confirmed(me, confirmed);
    }

    /// Every peer's reported rollback frontier — the host's view of who is
    /// keeping up and who is starved, for the 🌐 panel and the console line.
    pub fn rollback_frontiers(&self) -> Vec<(PeerId, u64)> {
        let mut v: Vec<(PeerId, u64)> =
            self.peer_confirmed.iter().map(|(p, t)| (*p, *t)).collect();
        v.sort_unstable();
        v
    }

    /// How many applied ticks we are still holding for each peer — a ring that
    /// keeps growing is a peer that has stopped confirming.
    pub fn rollback_backlog(&self) -> Vec<(PeerId, usize)> {
        self.rollback_rings.iter().map(|(p, r)| (*p, r.len())).collect()
    }

    /// Every peer's applied-tick inputs received since the last drain, for the
    /// driver to feed [`crate::Rollback::add_remote`].
    pub fn take_rollback_inputs(&mut self) -> Vec<(PeerId, u64, NetInput)> {
        std::mem::take(&mut self.rollback_in)
    }

    /// Client: a `RollbackStart` received since the last drain — `(roster,
    /// input delay, match seed)`. The driver (re)starts its rollback clock at 0
    /// on it.
    pub fn take_rollback_start(&mut self) -> Option<(Vec<PeerId>, u8, u64)> {
        self.rollback_start_in.take()
    }

    /// Publish this peer's state checksum for a confirmed tick (§6). On a
    /// client it goes to the host; on the host it enters the comparison
    /// directly.
    pub fn send_state_hash(&mut self, tick: u64, hash: u64) {
        match self.role {
            NetRole::Server => self.compare_state_hash(SERVER, tick, hash),
            NetRole::Client => {
                let msg = Msg::StateHash { tick, hash }.encode();
                self.transport.send(SERVER, Channel::Reliable, &msg);
            }
        }
    }

    /// Host: fold one peer's checksum in and, once everyone has reported for
    /// that tick, decide.
    ///
    /// Loud on mismatch, by design. The alternative — carrying on — is the one
    /// outcome the design refuses to allow: two machines playing a subtly
    /// different match, each convinced it is right.
    fn compare_state_hash(&mut self, peer: PeerId, tick: u64, hash: u64) {
        let expected = self.rollback_slots.len().max(1);
        let entry = self.state_hashes.entry(tick).or_default();
        if entry.iter().any(|(p, _)| *p == peer) {
            return;
        }
        entry.push((peer, hash));
        // With a referee running, a peer is judged the moment it reports —
        // against a simulation that never guessed, not against a quorum of
        // other players who might all be running the same modified build.
        if let Some(&truth) = self.referee_hashes.get(&tick) {
            if hash != truth {
                self.referee_faults.push((tick, peer));
                self.desyncs_in.push(tick);
                let msg = Msg::Desync { tick }.encode();
                for &p in &self.peers {
                    self.transport.send(p, Channel::Reliable, &msg);
                }
            }
            return;
        }
        if entry.len() < expected {
            return;
        }
        let agreed = entry.iter().all(|(_, h)| *h == hash);
        self.state_hashes.remove(&tick);
        // Anything older is moot now — a peer that never reported for it never
        // will, and holding the entries forever is a slow leak over a long set.
        self.state_hashes.retain(|t, _| *t > tick);
        if agreed {
            return;
        }
        self.desyncs_in.push(tick);
        let msg = Msg::Desync { tick }.encode();
        for &p in &self.peers {
            self.transport.send(p, Channel::Reliable, &msg);
        }
    }

    /// Ticks a desync was detected (host) or announced (client) for, since the
    /// last drain.
    pub fn take_desyncs(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.desyncs_in)
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
        let msg = Msg::Spawn { epoch: self.scene_epoch, id, node_ron: ron, owner }.encode();
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
        self.last_sent.retain(|(_, sid), _| *sid != id);
        self.last_synced.retain(|(_, sid, _, _), _| *sid != id);
        self.last_anim.retain(|(_, aid, _), _| *aid != id);
        self.interest_sets.forget_everywhere(id);
        world.despawn(e);
        let msg = Msg::Despawn { epoch: self.scene_epoch, id }.encode();
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
                Incoming::Disconnected(p, _) => self.drop_peer(p),
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
        // Rollback fan-out: every peer's recent applied-tick inputs, to every
        // peer, every tick. Sequenced-unreliable because the window is already
        // the redundancy — a lost packet costs nothing, and a retransmit would
        // arrive after the tick it was needed for.
        if self.rollback && !self.peers.is_empty() {
            let entries = self.rollback_fanout();
            if !entries.is_empty() {
                let msg = Msg::Inputs { entries }.encode();
                for &p in &self.peers {
                    self.transport.send(p, Channel::UnreliableSequenced, &msg);
                }
            }
        }
        // Input-timing feedback, a few times a second: each peer learns how
        // much runway its inputs have (auto-lead reads this client-side).
        if !self.peers.is_empty() && tick.is_multiple_of(INPUT_ACK_EVERY) {
            self.ping_id = self.ping_id.wrapping_add(1);
            let id = self.ping_id;
            let probe = Msg::Ping { id }.encode();
            let now = std::time::Instant::now();
            for i in 0..self.peers.len() {
                let p = self.peers[i];
                let margin = self.peer_margin.get(&p).map(|m| m.round() as i32).unwrap_or(0);
                let late = self.peer_late.get(&p).copied().unwrap_or(0);
                let ack = Msg::InputAck { margin, late }.encode();
                self.transport.send(p, Channel::UnreliableSequenced, &ack);
                // Unreliable on purpose: a probe that was retransmitted would
                // measure the retransmission, not the link.
                self.transport.send(p, Channel::Unreliable, &probe);
                self.pings_out.insert((id, p), now);
            }
            self.expire_pings(now);
        }
        if self.peers.is_empty() || !tick.is_multiple_of(SNAPSHOT_EVERY as u64) {
            return;
        }
        self.snap_count += 1;
        let keyframe = self.snap_count % KEYFRAME_EVERY == 1;
        if self.interest.enabled {
            self.send_interest_snapshots(world, tick, keyframe);
            return;
        }
        // Broadcast: one snapshot, encoded once, identical for everyone. Below
        // a few dozen players this is both cheaper and simpler than working out
        // who deserves what, which is why interest management is opt-in.
        let snapshot = self.build_snapshot(world, tick, keyframe);
        if let Some(msg) = snapshot {
            let bytes = msg.encode();
            for &p in &self.peers {
                self.transport.send(p, Channel::UnreliableSequenced, &bytes);
            }
        }
    }

    /// One snapshot per client, carrying only what that client is near enough
    /// to care about and only as much of it as its byte budget allows
    /// (`docs/netcode-design.md` §5.2, [`crate::interest`]).
    fn send_interest_snapshots(&mut self, world: &World, tick: u64, keyframe: bool) {
        let snaps_per_sec = if self.tick_dt > 0.0 {
            1.0 / (self.tick_dt * SNAPSHOT_EVERY as f32)
        } else {
            30.0
        };
        let budget = self.interest.budget_per_snapshot(snaps_per_sec);
        let (radius, hyst) = (self.interest.radius, self.interest.hysteresis);

        // Everything replicable this tick, gathered once and reused per peer —
        // the whole point is to scale with player count, so an O(players ×
        // entities) walk of the WORLD (rather than of this list) would put the
        // cost straight back.
        let mut all: Vec<Replicable> = Vec::new();
        for (e, rep) in world.query::<Replicated>() {
            if !rep.transform {
                continue;
            }
            let (Some(&id), Some(tr)) = (self.ent_to_net.get(&e), world.get::<Transform>(e))
            else {
                continue;
            };
            let pos = [tr.translation.x, tr.translation.y, tr.translation.z];
            all.push(Replicable { id, entity: e, rep: *rep, pos, rot: tr.rotation.to_array() });
        }

        for pi in 0..self.peers.len() {
            let peer = self.peers[pi];
            // Where this client is standing: its own avatar. A spectator has
            // none, and then nothing is far away rather than everything being —
            // it still pays the budget, so it degrades instead of going blind.
            let eye = all.iter().find(|r| r.rep.owner == Some(peer)).map(|r| r.pos);

            let mut relevant: std::collections::HashSet<u64> = std::collections::HashSet::new();
            let mut candidates: Vec<crate::interest::Candidate> = Vec::new();
            for r in &all {
                let (id, rep, pos, rot) = (&r.id, &r.rep, &r.pos, &r.rot);
                let owned = rep.owner == Some(peer);
                let distance = eye.map(|c| {
                    let (dx, dy, dz) = (pos[0] - c[0], pos[1] - c[1], pos[2] - c[2]);
                    (dx * dx + dy * dy + dz * dz).sqrt()
                });
                let held = self.interest_sets.get_mut(peer).is_live(*id);
                // Hysteresis applies only on the way OUT. Something already
                // being tracked gets to drift a little past the edge before it
                // goes quiet, so a node hovering on the boundary doesn't enter
                // and leave every single snapshot.
                let reach = if held { radius + hyst } else { radius };
                let near = distance.is_none_or(|d| d <= reach);
                if !(rep.always_relevant || owned || near) {
                    continue;
                }
                relevant.insert(*id);
                let base = self.last_sent.get(&(Some(peer), *id));
                let changed = base.is_none_or(|(p, r)| p != pos || r != rot);
                if !(keyframe || changed || !held) {
                    continue; // relevant, but this client is already current
                }
                candidates.push(crate::interest::Candidate {
                    id: *id,
                    distance,
                    changed,
                    is_player: rep.owner.is_some(),
                    is_owned: owned,
                    always: rep.always_relevant,
                    cost: SNAP_ENTRY_BYTES + if rep.physics { SNAP_BODY_BYTES } else { 0 },
                });
            }

            // What this client holds but may no longer hear about. Runtime
            // spawns are despawned there (the server can recreate them from
            // `spawned_docs` on re-entry); scene-authored nodes are only muted,
            // because the client already has them from the scene file and
            // nothing could bring one back.
            let stale = self.interest_sets.get_mut(peer).stale(&relevant);
            for id in stale {
                self.interest_sets.get_mut(peer).forget(id);
                self.last_sent.retain(|(a, sid), _| !(*a == Some(peer) && *sid == id));
                if self.spawned_docs.contains_key(&id) {
                    let msg = Msg::Despawn { epoch: self.scene_epoch, id }.encode();
                    self.transport.send(peer, Channel::Reliable, &msg);
                }
            }

            let chosen = self.interest_sets.get_mut(peer).choose(&candidates, radius, budget);
            // What this client's snapshot cost, before the entries are built —
            // `candidates` is everything that wanted a turn, `chosen` is what
            // got one, and the difference is the budget doing its job.
            self.interest_stats.insert(
                peer,
                crate::interest::InterestStat {
                    relevant: relevant.len(),
                    sent: chosen.len(),
                    deferred: candidates.len().saturating_sub(chosen.len()),
                    bytes: candidates
                        .iter()
                        .filter(|c| chosen.contains(&c.id))
                        .map(|c| c.cost)
                        .sum(),
                },
            );
            let mut entries = Vec::new();
            for id in &chosen {
                let Some(r) = all.iter().find(|r| r.id == *id) else { continue };
                self.last_sent.insert((Some(peer), *id), (r.pos, r.rot));
                let body =
                    r.rep.physics.then(|| self.body_states.get(&r.entity).copied()).flatten();
                entries.push(SnapEntry {
                    id: *id,
                    pos: r.pos,
                    rot: r.rot,
                    vel: body.map(|(v, _)| v),
                    grounded: body.map(|(_, g)| g),
                });
            }

            // `synced` vars and animators ride the relevant set too, but are
            // not rationed: they are small, they are usually idle, and a stale
            // gameplay flag is a correctness problem where a stale position is
            // only a cosmetic one.
            let synced = self.build_synced_for(peer, &relevant, keyframe);
            let anims = self.collect_anim_entries(tick, keyframe, Some(peer), Some(&relevant));
            if entries.is_empty() && synced.is_empty() && anims.is_empty() && !keyframe {
                continue;
            }
            let msg = Msg::Snapshot {
                epoch: self.scene_epoch,
                tick,
                keyframe,
                entries,
                synced,
                anims,
            }
            .encode();
            self.transport.send(peer, Channel::UnreliableSequenced, &msg);
        }
    }

    /// Per-peer `synced` diff, restricted to what that peer can see.
    fn build_synced_for(
        &mut self,
        peer: PeerId,
        relevant: &std::collections::HashSet<u64>,
        keyframe: bool,
    ) -> Vec<SyncedEntry> {
        let mut out = Vec::new();
        for (e, script, vars) in &self.synced_now {
            let Some(&id) = self.ent_to_net.get(e) else { continue };
            if !relevant.contains(&id) {
                continue;
            }
            let mut changed = Vec::new();
            for (k, v) in vars {
                let key = (Some(peer), id, script.clone(), k.clone());
                if keyframe || self.last_synced.get(&key) != Some(v) {
                    self.last_synced.insert(key, v.clone());
                    changed.push((k.clone(), v.clone()));
                }
            }
            if !changed.is_empty() {
                out.push(SyncedEntry { id, script: script.clone(), vars: changed });
            }
        }
        out
    }

    fn server_message(&mut self, world: &World, from: PeerId, msg: Msg, tick: u64) {
        match msg {
            Msg::Hello { proto, input_map } => {
                if proto != PROTO_VERSION {
                    let refuse = Msg::Refused {
                        reason: format!("protocol {proto} != {PROTO_VERSION}"),
                    };
                    self.transport.send(from, Channel::Reliable, &refuse.encode());
                    return;
                }
                // Input commands index actions by their position in input.ron.
                // A joiner whose map has a different shape would decode every
                // command as the wrong actions — and nothing would report an
                // error, it would just play wrong. Refuse instead.
                if input_map != self.input_map_hash {
                    let refuse = Msg::Refused {
                        reason: "input.ron differs from the host's — actions are indexed by \
                                 their order in the map, so the two builds must match"
                            .to_string(),
                    };
                    self.transport.send(from, Channel::Reliable, &refuse.encode());
                    return;
                }
                self.peers.push(from);
                self.events.push(NetEvent::PeerJoined(from));
                let welcome = Msg::Welcome {
                    peer: from,
                    tick,
                    snapshot_every: SNAPSHOT_EVERY,
                    scene: self.scene.clone(),
                    epoch: self.scene_epoch,
                    input_delay: self.input_delay,
                };
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
                        epoch: self.scene_epoch,
                        id,
                        node_ron: ron.clone(),
                        owner: *owner,
                    })
                    .collect();
                for s in spawns {
                    self.transport.send(from, Channel::Reliable, &s.encode());
                }
                // The joiner's baseline. Under interest management there is no
                // such thing as "the whole world's state" to hand someone —
                // sending it would blast every entity in the map at exactly the
                // moment the feature exists to stop that. A client with no
                // interest set yet counts everything relevant as never-seen, so
                // its neighbourhood arrives in full over the next few
                // snapshots, budgeted, nearest first.
                if !self.interest.enabled
                    && let Some(kf) = self.build_full_snapshot(world, tick)
                {
                    // Reliable: the joiner MUST get its baseline.
                    self.transport.send(from, Channel::Reliable, &kf.encode());
                }
                if self.rollback {
                    // A new player means a new roster and a new match clock —
                    // every peer restarts at tick 0 together.
                    let (delay, seed) = (self.input_delay, self.rollback_seed);
                    self.set_rollback(true, delay, seed);
                }
            }
            Msg::Rpc { name, args, tick: perceived, .. } => {
                // Stamp the true sender — never trust the payload's claim. The
                // perceived tick is clamped at rewind time, not here.
                self.rpcs_in.push(ReceivedRpc { name, args, sender: from, tick: perceived });
            }
            Msg::Input { entries, confirmed } => {
                if self.rollback {
                    // Rollback: the host doesn't CONSUME a peer's input, it
                    // relays it. Everyone simulates everyone, so an input is
                    // only useful once every peer has it.
                    //
                    // Their frontier first: it is what tells us we may stop
                    // re-sending a tick, and applying it before the arrivals
                    // keeps the ring from being trimmed against a stale floor.
                    self.note_peer_confirmed(from, confirmed);
                    for cmd in entries {
                        self.note_rollback_input(from, cmd);
                    }
                    return;
                }
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
            Msg::Ping { id } => {
                let pong = Msg::Pong { id }.encode();
                self.transport.send(from, Channel::Unreliable, &pong);
            }
            Msg::Pong { id } => self.note_pong(id, from),
            Msg::StateHash { tick, hash } => self.compare_state_hash(from, tick, hash),
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
            self.interest_sets.drop_peer(p);
            self.interest_stats.remove(&p);
            self.peer_rtt.remove(&p);
            // A peer that left must stop holding the retention floor down —
            // otherwise every ring grows to its cap for the rest of the session
            // waiting on a frontier that will never move again.
            self.peer_confirmed.remove(&p);
            self.rollback_rings.remove(&p);
            self.pings_out.retain(|(_, q), _| *q != p);
            self.last_sent.retain(|(a, _), _| *a != Some(p));
            self.last_synced.retain(|(a, _, _, _), _| *a != Some(p));
            self.last_anim.retain(|(a, _, _), _| *a != Some(p));
            self.events.push(NetEvent::PeerLeft(p));
            let left = Msg::PeerLeft { peer: p }.encode();
            for &q in &self.peers {
                self.transport.send(q, Channel::Reliable, &left);
            }
            if self.rollback {
                let (delay, seed) = (self.input_delay, self.rollback_seed);
                self.set_rollback(true, delay, seed);
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
            let changed =
                self.last_sent.get(&(None, id)).is_none_or(|(p, r)| *p != pos || *r != rot);
            if keyframe || changed {
                self.last_sent.insert((None, id), (pos, rot));
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
                let key = (None, id, script.clone(), k.clone());
                if keyframe || self.last_synced.get(&key) != Some(v) {
                    self.last_synced.insert(key, v.clone());
                    changed_vars.push((k.clone(), v.clone()));
                }
            }
            if !changed_vars.is_empty() {
                synced.push(SyncedEntry { id, script: script.clone(), vars: changed_vars });
            }
        }
        let anims = self.collect_anim_entries(tick, keyframe, None, None);
        if entries.is_empty() && synced.is_empty() && anims.is_empty() && !keyframe {
            return None;
        }
        Some(Msg::Snapshot { epoch: self.scene_epoch, tick, keyframe, entries, synced, anims })
    }

    /// Encode the animator states that need sending: everything on a keyframe,
    /// else only the SURPRISED ones — a changed state/weight/speed, or a clock
    /// the time predictor couldn't foresee (a seek, a hitch). An undisturbed
    /// looping animation costs zero bytes here.
    fn collect_anim_entries(
        &mut self,
        tick: u64,
        keyframe: bool,
        who: Option<PeerId>,
        relevant: Option<&std::collections::HashSet<u64>>,
    ) -> Vec<AnimEntry> {
        let mut out = Vec::new();
        for (e, sub, speed, layers) in &self.anims_now {
            let Some(&id) = self.ent_to_net.get(e) else { continue };
            if relevant.is_some_and(|r| !r.contains(&id)) {
                continue;
            }
            let speed_q = AnimEntry::quantize_speed(*speed);
            let wire: Vec<AnimLayerWire> = layers
                .iter()
                .take(MAX_ANIM_LAYERS)
                .map(|l| AnimLayerWire::quantize(l.state, l.t, l.weight))
                .collect();
            let dirty = match self.last_anim.get(&(who, id, *sub)) {
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
                    (who, id, *sub),
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
                out.push(AnimEntry { id, sub: *sub, speed: speed_q, layers: wire });
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
            .filter_map(|(e, sub, speed, layers)| {
                let &id = self.ent_to_net.get(e)?;
                Some(AnimEntry {
                    id,
                    sub: *sub,
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
        Some(Msg::Snapshot {
            epoch: self.scene_epoch,
            tick,
            keyframe: true,
            entries,
            synced,
            anims,
        })
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
                Incoming::Disconnected(_, why) => {
                    self.connected = false;
                    // Whatever the transport knew. "server closed" is the
                    // honest fallback for a link that simply ended, and a poor
                    // description of a lobby code that was never valid — which
                    // is what every refused join used to report.
                    self.join_state = match &why {
                        Some(r) => JoinState::Refused(r.clone()),
                        None => JoinState::Refused("the connection ended".into()),
                    };
                    self.events.push(NetEvent::Disconnected(
                        why.unwrap_or_else(|| "server closed".into()),
                    ));
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
        // Probe the host on the same cadence the host probes us, so a client's
        // own ping display is honest through a relay as well.
        if self.connected && self.client_ticks.is_multiple_of(INPUT_ACK_EVERY) {
            self.ping_id = self.ping_id.wrapping_add(1);
            let id = self.ping_id;
            self.transport.send(SERVER, Channel::Unreliable, &Msg::Ping { id }.encode());
            let now = std::time::Instant::now();
            self.pings_out.insert((id, SERVER), now);
            self.expire_pings(now);
        }
        if self.connected && !self.input_window.is_empty() {
            let msg = Msg::Input {
                entries: self.input_window.iter().cloned().collect(),
                confirmed: self.rollback_confirmed,
            };
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
        self.anim_bufs.retain(|key, buf| {
            let Some(&e) = self.net_to_ent.get(&key.0) else {
                return false; // entity gone — drop the buffer
            };
            let delay = self
                .interp
                .get(&key.0)
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
            if due.is_none() && !self.anim_started.contains(key) && !buf.is_empty() {
                due = buf.pop_front().map(|(_, _, en)| en);
            }
            if let Some(en) = due {
                self.anim_started.insert(*key);
                self.anims_due.push((e, en));
            }
            true
        });
    }

    /// Is this node simulated HERE rather than received?
    ///
    /// True only for a `Rollback` node in a live rollback session — every peer
    /// simulates it from the shared input stream, so the host's snapshot of it
    /// is not authority, it is a second opinion arriving a round trip late. Let
    /// it land and the node is tugged between the driver's tick pose and an
    /// interpolated pose from the past, every single frame.
    ///
    /// Deliberately gated on the SESSION's rollback flag rather than on the
    /// mode alone. Before `RollbackStart` there is no driver yet: the fighters
    /// are parked and snapshot-driven exactly like anything else, which is what
    /// puts a joining client's scene in the right place before the match
    /// begins. The flag flips at the same moment the driver takes over.
    fn driven_locally(&self, world: &World, id: u64) -> bool {
        if !self.rollback {
            return false;
        }
        self.net_to_ent
            .get(&id)
            .and_then(|&e| world.get::<Replicated>(e))
            .is_some_and(|rep| rep.mode.is_rollback())
    }

    /// Drop everything buffered FOR locally-simulated nodes. Called when a
    /// rollback session starts: samples that arrived a moment before the match
    /// did are still in the interpolation buffers, and `apply_interpolation`
    /// keeps applying the newest sample it holds whether or not new ones come —
    /// so refusing to buffer any more is not enough on its own.
    fn drop_locally_driven_buffers(&mut self, world: &World) {
        let ids: Vec<u64> = self
            .net_to_ent
            .iter()
            .filter(|(_, e)| {
                world.get::<Replicated>(**e).is_some_and(|rep| rep.mode.is_rollback())
            })
            .map(|(&id, _)| id)
            .collect();
        for id in &ids {
            // Cleared, not removed: the buffer carries the node's own `interp`
            // and `interp_delay`, which it would silently lose to defaults if
            // the session ever stopped being a rollback one.
            if let Some(buf) = self.interp.get_mut(id) {
                buf.samples.clear();
            }
        }
        self.anim_bufs.retain(|(id, _), _| !ids.contains(id));
        self.anim_started.retain(|(id, _)| !ids.contains(id));
        self.anims_due.retain(|(e, _)| {
            !world.get::<Replicated>(*e).is_some_and(|rep| rep.mode.is_rollback())
        });
    }

    fn client_message(&mut self, world: &mut World, msg: Msg) {
        match msg {
            Msg::Welcome { peer, tick, scene, epoch, input_delay, .. } => {
                self.connected = true;
                self.join_state = JoinState::Joined;
                self.my_peer = Some(peer);
                self.welcome_tick = Some(tick);
                self.scene_epoch = epoch;
                self.input_delay = input_delay;
                if !scene.is_empty() {
                    // The session's scene: the driver compares against what it
                    // has loaded, switches if needed, and rebinds either way.
                    self.scene = scene.clone();
                    self.scene_switch_in = Some(scene);
                    self.scene_pending = true;
                }
                self.events.push(NetEvent::Connected);
            }
            Msg::Refused { reason } => {
                self.connected = false;
                self.events.push(NetEvent::Disconnected(reason));
            }
            Msg::Scene { epoch, scene } => {
                self.scene_epoch = epoch;
                self.scene = scene.clone();
                self.scene_switch_in = Some(scene);
                // Everything buffered belongs to the OLD scene; scene-scoped
                // messages stay dropped until the driver rebinds.
                self.scene_pending = true;
                self.interp.clear();
                self.anim_bufs.clear();
                self.anim_started.clear();
                self.anims_due.clear();
                self.predicted_in.clear();
                // A scene switch ENDS the match. The tick origin, the roster's
                // slot order and the state ring are all indexed against the
                // scene that just went away, and the new scene's fighters are
                // different nodes. If it has any, the host announces a fresh
                // `RollbackStart` — with a new seed and a new tick 0 — right
                // behind this message, and the session starts again from there.
                self.rollback = false;
                self.rollback_slots.clear();
                self.rollback_rings.clear();
                self.rollback_in.clear();
                self.peer_confirmed.clear();
                self.rollback_confirmed = 0;
                self.state_hashes.clear();
            }
            Msg::Spawn { epoch, id, node_ron, owner } => {
                if epoch != self.scene_epoch || self.scene_pending {
                    return; // another scene's spawn — stale or early
                }
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
            Msg::Despawn { epoch, id } => {
                if epoch != self.scene_epoch || self.scene_pending {
                    return;
                }
                if let Some(e) = self.net_to_ent.remove(&id) {
                    self.ent_to_net.remove(&e);
                    self.interp.remove(&id);
                    self.anim_bufs.retain(|(bid, _), _| *bid != id);
                    self.anim_started.retain(|(bid, _)| *bid != id);
                    self.despawned_in.push(e.index());
                    world.despawn(e);
                }
            }
            Msg::Snapshot { epoch, tick, entries, synced, anims, .. } => {
                if epoch != self.scene_epoch || self.scene_pending {
                    return; // another scene's state — the id map doesn't apply
                }
                if tick <= self.latest_server_tick && tick != 0 && !entries.is_empty() {
                    // Sequenced channel already drops stale, but the reliable
                    // late-join keyframe can race a newer unreliable snapshot.
                    if tick < self.latest_server_tick {
                        return;
                    }
                }
                self.latest_server_tick = self.latest_server_tick.max(tick);
                for an in anims {
                    if self.driven_locally(world, an.id) {
                        continue;
                    }
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
                    let buf = self.anim_bufs.entry((an.id, an.sub)).or_default();
                    buf.push_back((tick, self.client_ticks, an));
                    while buf.len() > MAX_SAMPLES {
                        buf.pop_front();
                    }
                }
                for en in entries {
                    if self.driven_locally(world, en.id) {
                        continue;
                    }
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
                    if self.driven_locally(world, s.id) {
                        continue;
                    }
                    if let Some(&e) = self.net_to_ent.get(&s.id) {
                        self.synced_in.push((e, s.script, s.vars));
                    }
                }
            }
            Msg::Ping { id } => {
                let pong = Msg::Pong { id }.encode();
                self.transport.send(SERVER, Channel::Unreliable, &pong);
            }
            Msg::Pong { id } => self.note_pong(id, SERVER),
            Msg::Rpc { name, args, sender, tick } => {
                self.rpcs_in.push(ReceivedRpc { name, args, sender, tick });
            }
            Msg::PeerJoined { peer } => self.events.push(NetEvent::PeerJoined(peer)),
            Msg::PeerLeft { peer } => self.events.push(NetEvent::PeerLeft(peer)),
            Msg::RollbackStart { peers, input_delay, seed } => {
                // The host set the roster and the parameters; this is also the
                // shared tick origin, so the driver restarts at 0 on it.
                self.rollback = true;
                self.input_delay = input_delay.min(crate::rollback::MAX_DELAY);
                self.rollback_slots = peers.clone();
                self.rollback_seed = seed;
                self.rollback_rings.clear();
                self.rollback_in.clear();
                self.peer_confirmed.clear();
                self.rollback_confirmed = 0;
                // The fixed delay replaces the adaptive lead — leaving both on
                // has one mechanism shifting the stamps the other assumes are
                // stable (§0.5.1).
                self.auto_lead = false;
                self.stamp_offset = 0;
                self.input_window.clear();
                self.drop_locally_driven_buffers(world);
                self.rollback_start_in = Some((peers, self.input_delay, seed));
            }
            Msg::Inputs { entries } => {
                let me = self.my_peer;
                for (peer, cmd) in entries {
                    // Our own input echoed back: we are the authority on it and
                    // already simulated with it.
                    if Some(peer) == me {
                        continue;
                    }
                    self.note_rollback_input(peer, cmd);
                }
            }
            Msg::Desync { tick } => self.desyncs_in.push(tick),
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
            // Belt and suspenders over the ingest guard in `client_message`:
            // this is the line that would actually move a locally-simulated
            // fighter, so it refuses on its own terms rather than trusting that
            // nothing upstream ever buffers one.
            if self.rollback
                && world.get::<Replicated>(e).is_some_and(|rep| rep.mode.is_rollback())
            {
                continue;
            }
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
