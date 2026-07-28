//! The rollback driver (`docs/rollback-netcode-design.md` §7 P3) — the half of
//! rollback that actually runs a simulation.
//!
//! [`floptle_net::Rollback`] is the bookkeeping brain: it decides *whether* and
//! *how far* to roll back. This module owns everything it deliberately doesn't
//! — the state ring, the re-simulation loop, and the side-effect gate — because
//! all of that means running `fixedUpdate` and stepping physics bodies.
//!
//! It is a generalization of the predictor's replay loop (`net.rs`): where that
//! rewinds ONE entity to the server's word and replays its unacknowledged
//! inputs, this rewinds EVERY rollback node together and replays every peer's
//! inputs, in a fixed order, with all three kinds of per-tick state restored
//! around it.
//!
//! ## The three state kinds, and why all three
//!
//! A saved tick (§2.4) is physics + script + input, and dropping any one of them
//! produces a rollback that looks like it works:
//!
//! - **Physics** — [`floptle_physics::BodySnapshot`] plus the node `Transform`
//!   (rotation isn't in the body snapshot).
//! - **Script** — each script's `snapshot()` value, deep-copied by the engine in
//!   both directions so a replay cannot corrupt the state it restored from.
//! - **Input tick-state** — [`floptle_input::InputSystem::snapshot_tick`]. This
//!   is the one that's easy to forget: `consume` records a *decision* a script
//!   made, not a function of the inputs, so it cannot be recomputed — only
//!   restored. Skip it and a buffered punch fires twice, or a quarter-circle
//!   that matched once fails to match on the replay. Neither shows up as an
//!   error; both show up as a desync.
//!
//! ## Live and replayed ticks run the SAME code
//!
//! [`RollbackDriver::advance`] and the replay loop both end in
//! [`RollbackDriver::simulate_tick`]. That is not tidiness — it is the whole
//! correctness argument. The acceptance test asserts that re-simulating a span
//! reproduces an uninterrupted run *bit for bit*, and that can only hold by
//! construction: same input injection, same hook order, same physics call.
//! It is also why the driver takes the rollback bodies away from the whole-world
//! physics step ([`floptle_physics::Sim::set_driven_bodies`]) rather than
//! stepping them one way live and another way in a replay.

use std::collections::{HashSet, VecDeque};

use floptle_core::math::Vec3;
use floptle_core::transform::Transform;
use floptle_core::{Entity, World};
use floptle_input::TickSnapshot;
use floptle_net::{Correction, NetInput, PeerId, ResolvedInput, Rollback};
use floptle_physics::{BodySnapshot, Sim};
use floptle_script::{ScriptHost, ScriptState};

/// Saved ticks kept beyond the depth cap.
///
/// The cap bounds how far a correction may reach back; the margin covers the
/// ticks between "the input arrived" and "the driver got around to resolving
/// it", plus the confirmed-frontier anchor the checksums hash (§2.1). Cheap
/// insurance: a saved tick is a few KB, and a ring that comes up one entry
/// short turns a routine correction into an unrecoverable desync.
pub const RING_MARGIN: u32 = 4;

/// One node the driver simulates every tick on every peer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RollbackNode {
    pub entity: Entity,
    pub eid: u32,
    /// The local player slot this node's inputs are injected into. Scripts read
    /// it as `input.player(slot + 1)` — the Fofighter `params.player`
    /// convention, so no script changes.
    pub slot: u8,
}

/// The engine state of one tick, as it stood **before** that tick simulated.
///
/// Keyed that way because a [`Correction`] names the earliest tick whose
/// simulation is now known to be wrong: the driver restores the state saved for
/// that tick and re-simulates *from* it.
#[derive(Clone)]
struct SavedTick {
    tick: u64,
    /// Per node, parallel to [`RollbackDriver::nodes`]. `None` = the node has no
    /// physics body (a script-only fighter, or one that hasn't spawned yet).
    bodies: Vec<Option<BodySnapshot>>,
    transforms: Vec<Option<Transform>>,
    scripts: Vec<ScriptState>,
    input: TickSnapshot,
}

/// Everything one tick needs, borrowed for the call. The driver holds no
/// references to the world between ticks, so the editor can keep owning all of
/// it and a headless test can assemble it from nothing.
pub struct Ctx<'a> {
    pub world: &'a mut World,
    pub sim: &'a mut Sim,
    pub host: &'a mut ScriptHost,
    /// The fixed gameplay-tick delta (1/60 s).
    pub step: f32,
}

/// A per-tick rollback simulation over the scene's `Rollback` nodes.
pub struct RollbackDriver {
    /// Input rings, delay, prediction, the confirmed frontier and mispredict
    /// detection. Public because the Hub panel and `net.rollback*` read its
    /// counters straight out (§7 P6).
    pub net: Rollback,
    /// The nodes this driver simulates, in **live scene order** (§0.5.4). Both
    /// machines and every replay must run their hooks in the same order, or two
    /// fighters resolving a trade would resolve it differently on each screen.
    nodes: Vec<RollbackNode>,
    /// Peer ids by slot: `slots[n]` owns player slot `n`. Host-assigned and
    /// carried in `Welcome`, so every peer agrees who is player 1.
    slots: Vec<PeerId>,
    ring: VecDeque<SavedTick>,
    /// The earliest tick a correction has invalidated, resolved at the top of
    /// the next [`RollbackDriver::advance`]. Several corrections between two
    /// ticks collapse into the earliest — one replay covers them all.
    pending: Option<u64>,
    /// True while the local sim is waiting for input rather than guessing past
    /// the depth cap (§2.3). The editor shows a connection indicator on it.
    pub stalled: bool,
    /// The newest tick a checksum has been published for (§6).
    last_checksum: u64,
    /// The match's RNG seed (§3), host-chosen and identical on every peer.
    seed: u64,
    /// Diagnostics: ticks re-simulated since the session started. Divided by
    /// `net.corrections` it is the average rollback depth a player actually
    /// felt.
    pub resimulated_ticks: u64,
    /// A desync has been reported for this session. Sticky: once two peers
    /// have disagreed, everything after it is suspect, and a readout that
    /// flicked back to green would be a lie.
    pub desynced: bool,
    /// Problems that must reach the Console rather than being swallowed — a
    /// correction reaching past the ring, an input snapshot that no longer
    /// fits, a `Rollback` node with no rollback hooks. Drained by the driver's
    /// owner.
    pub faults: Vec<String>,
}

impl RollbackDriver {
    /// A session where `peers` (including `local`) play in slot order, applying
    /// local input `delay` ticks after it is sampled. `seed` is the host's match
    /// seed — what `net.random()` draws from (§3).
    pub fn new(local: PeerId, peers: Vec<PeerId>, delay: u8, seed: u64) -> Self {
        Self {
            seed,
            slots: peers.clone(),
            net: Rollback::new(local, peers, delay),
            nodes: Vec::new(),
            ring: VecDeque::new(),
            pending: None,
            stalled: false,
            desynced: false,
            last_checksum: 0,
            resimulated_ticks: 0,
            faults: Vec::new(),
        }
    }

    pub fn nodes(&self) -> &[RollbackNode] {
        &self.nodes
    }

    /// Every entity the driver simulates — what the caller excludes from the
    /// global script passes and hands to [`Sim::set_driven_bodies`].
    pub fn eids(&self) -> HashSet<u32> {
        self.nodes.iter().map(|n| n.eid).collect()
    }

    /// How deep the ring currently reaches, in ticks. Diagnostics.
    pub fn ring_depth(&self) -> usize {
        self.ring.len()
    }

    /// The newest confirmed tick this peer has published a checksum for (§6).
    pub fn last_checksum(&self) -> u64 {
        self.last_checksum
    }

    /// Roughly how much the state ring occupies, in bytes — so "why is this
    /// using memory" has an answer in the multiplayer panel.
    pub fn ring_bytes(&self) -> usize {
        self.ring
            .iter()
            .map(|s| {
                s.scripts.iter().map(ScriptState::size_hint).sum::<usize>()
                    + s.bodies.len() * std::mem::size_of::<BodySnapshot>()
                    + s.transforms.len() * std::mem::size_of::<Transform>()
            })
            .sum()
    }

    /// (Re)discover the scene's `Rollback` nodes, in scene order, and take
    /// their bodies over from the whole-world physics step.
    ///
    /// Call at session start and after any scene change. Order comes from the
    /// `Transform` column, which the scene loader fills in file order — the
    /// same order the global script pass visits, so a replay's relative
    /// execution order matches the live tick's by construction.
    pub fn rebind(&mut self, world: &World, sim: &mut Sim, host: &ScriptHost) {
        self.nodes.clear();
        for (e, _) in world.query::<Transform>() {
            let is_rollback = world
                .get::<floptle_core::Replicated>(e)
                .is_some_and(|r| r.mode.is_rollback());
            if !is_rollback {
                continue;
            }
            let slot = self.nodes.len() as u8;
            self.nodes.push(RollbackNode { entity: e, eid: e.index(), slot });
        }
        for n in &self.nodes {
            let synced = host.synced_kinds_on(n.eid);
            if !synced.is_empty() {
                let name = world
                    .get::<floptle_core::Name>(n.entity)
                    .map(|x| x.0.clone())
                    .unwrap_or_else(|| format!("#{}", n.eid));
                self.faults.push(format!(
                    "\"{name}\" is a Rollback node whose script(s) {} also declare `synced` \
                     vars. Those are two owners for one value: rollback says this machine \
                     simulates it and a correction may rewrite it, `synced` says the host \
                     owns it and ships it. Which one wins comes down to arrival timing. Move \
                     the value into snapshot()/restore() and drop the `synced`.",
                    synced.join(", "),
                ));
            }
            if !host.has_rollback_hooks(n.eid) {
                let name = world
                    .get::<floptle_core::Name>(n.entity)
                    .map(|x| x.0.clone())
                    .unwrap_or_else(|| format!("#{}", n.eid));
                self.faults.push(format!(
                    "\"{name}\" is a Rollback node but none of its scripts define \
                     snapshot()/restore() — it will NOT be rolled back. That is right for \
                     cosmetics and wrong for gameplay: a correction will leave its state on \
                     the timeline that didn't happen."
                ));
            }
        }
        // One player slot per fighter, or the extras read neutral forever. It is
        // deterministic (`set_tick_state` no-ops identically on every peer, so
        // checksums stay green) and therefore completely silent: a fighter that
        // simply never moves, in a match that never complains. Say it here.
        let players = host.input_system().borrow().players();
        if self.nodes.len() > players {
            self.faults.push(format!(
                "input.ron declares {players} player slot(s) but the scene has {} Rollback \
                 node(s) — slot {players} and up have nowhere to read input from and will \
                 stand still all match. Raise `players` in the project's input map.",
                self.nodes.len(),
            ));
        }
        sim.set_driven_bodies(&self.eids());
        // Every peer simulates every rollback node, so every peer's copy of the
        // body has to be awake. On a client the session parks replicated bodies
        // when it joins (snapshots own them) — these ones it doesn't, and this
        // is the moment we take them back. Stated here rather than left to the
        // session because the order of `Connected`, the `Welcome` and
        // `RollbackStart` inside one client tick is subtle, and an inert
        // fighter is a very quiet way to fail.
        for n in &self.nodes {
            sim.set_body_active(n.eid, true);
        }
        // A rebind invalidates every saved tick: the ring is indexed by node
        // position, and the node list just changed.
        self.ring.clear();
        self.pending = None;
    }

    /// Hand the bodies back to the whole-world step and forget the ring — the
    /// session ended, or Play stopped.
    pub fn release(&mut self, sim: &mut Sim) {
        sim.set_driven_bodies(&HashSet::new());
        self.nodes.clear();
        self.ring.clear();
        self.pending = None;
    }

    /// Which slot a peer plays in, if they are in this session.
    pub fn slot_of(&self, peer: PeerId) -> Option<u8> {
        self.slots.iter().position(|p| *p == peer).map(|i| i as u8)
    }

    /// The state checksum for a saved tick (§6): the body snapshots and script
    /// state the driver holds for it, folded into one FNV-1a digest.
    ///
    /// Script state is hashed through [`floptle_net::NetValue::canonical_hash`],
    /// which sorts table pairs first. Without that, two peers in perfect
    /// agreement would report a desync roughly every time Lua handed them one
    /// table's keys in a different order — an alarm that cries wolf is worse
    /// than no alarm, because everyone learns to ignore it.
    ///
    /// Transforms are deliberately NOT hashed. Rotation on a fighter is derived
    /// presentation (which way the model faces), and a checksum that fires on
    /// divergence the simulation cannot feel is the same cried wolf.
    pub fn state_hash(&self, tick: u64) -> Option<u64> {
        let s = self.ring.iter().find(|s| s.tick == tick)?;
        let mut h = floptle_net::Fnv::new();
        h.eat(&tick.to_le_bytes());
        for (i, node) in self.nodes.iter().enumerate() {
            h.eat(&node.eid.to_le_bytes());
            match s.bodies.get(i).and_then(|b| b.as_ref()) {
                Some(b) => {
                    h.eat(b"B");
                    for v in [b.pos.x, b.pos.y, b.pos.z] {
                        h.eat(&v.to_bits().to_le_bytes());
                    }
                    for v in [b.vel.x, b.vel.y, b.vel.z] {
                        h.eat(&v.to_bits().to_le_bytes());
                    }
                    h.eat(&[b.grounded as u8]);
                }
                None => h.eat(b"-"),
            }
            if let Some(state) = s.scripts.get(i) {
                // Sorted by kind so two peers fold the same node's scripts in
                // the same order regardless of how the host's map iterated.
                let mut kinds: Vec<&(String, floptle_net::NetValue)> = state.entries.iter().collect();
                kinds.sort_by(|a, b| a.0.cmp(&b.0));
                for (kind, v) in kinds {
                    h.eat(kind.as_bytes());
                    h.eat(&v.canonical_hash().to_le_bytes());
                }
            }
        }
        Some(h.0)
    }

    /// The next checksum owed, if one is (§6: every `CHECKSUM_EVERY` confirmed
    /// ticks). Returns `(tick, hash)` once per tick and never repeats one.
    ///
    /// The tick is picked deterministically from the confirmed frontier — the
    /// largest multiple of `CHECKSUM_EVERY` at or below it — so two peers
    /// choose the same one without negotiating. If the frontier jumped past a
    /// multiple whose state has already fallen off the ring, that round is
    /// simply skipped: a missed checksum is a smaller problem than stalling a
    /// match to take one.
    pub fn due_checksum(&mut self) -> Option<(u64, u64)> {
        let confirmed = self.net.confirmed();
        if confirmed < floptle_net::CHECKSUM_EVERY {
            return None;
        }
        let tick = confirmed - confirmed % floptle_net::CHECKSUM_EVERY;
        if tick <= self.last_checksum {
            return None;
        }
        let hash = self.state_hash(tick)?;
        self.last_checksum = tick;
        Some((tick, hash))
    }

    /// Restart the match: a new roster, a new delay, and tick 0 again.
    ///
    /// Called on `Msg::RollbackStart`, which every peer receives — that shared
    /// moment is what gives the session a shared tick origin, and it is why v1
    /// does not support joining a rollback match in progress.
    pub fn restart(&mut self, local: PeerId, peers: Vec<PeerId>, delay: u8, seed: u64) {
        let max_depth = self.net.max_depth;
        self.seed = seed;
        self.slots = peers.clone();
        self.net = Rollback::new(local, peers, delay);
        self.net.max_depth = max_depth;
        self.ring.clear();
        self.pending = None;
        self.stalled = false;
        self.last_checksum = 0;
        self.resimulated_ticks = 0;
        // `desynced` deliberately SURVIVES a restart. A restart is how a roster
        // change re-syncs the clock, not a fresh install: if the last match
        // forked, the panel keeps saying so until the session actually ends,
        // because "we desynced and then quietly restarted" is exactly the state
        // a player needs to be told about. `net_rollback_stop` clears it by
        // dropping the driver.
    }

    /// May this frame sample the local pad, and for which tick?
    ///
    /// `None` while stalled, and that is the whole point: a stall leaves the
    /// frontier where it is, so the next frame would otherwise sample the SAME
    /// tick a second time. A tick may only ever be sampled once. The second
    /// sample overwrites the first locally, while on the wire the fan-out's
    /// per-`(peer, tick)` dedup drops it — so this machine simulates the tick
    /// with the newer input and every other machine simulates it with the
    /// older one. That is a desync on every stall, which is to say on every
    /// rough patch of the connection.
    ///
    /// Not sampling cannot stall the session shut: while stalled our own newest
    /// input is `current + 1 + delay`, far past the confirmed frontier the
    /// stall is waiting on, so the peer we are waiting for is never us.
    pub fn sample_tick(&self) -> Option<u64> {
        let next = self.net.current() + 1;
        (!self.net.should_stall(next)).then_some(next)
    }

    /// Record the local player's sampled input. Returns the tick it will
    /// APPLY on (`sampled + delay`) — which is what goes on the wire, so peers
    /// never have to know our delay.
    pub fn add_local(&mut self, sampled: u64, input: NetInput) -> u64 {
        let applied = self.net.applied_tick(sampled);
        // Our own input arriving for a tick we already ran means that tick was
        // simulated from a GUESS at our own pad — and nothing will ever correct
        // it, because corrections are only raised for other peers. We would
        // ship the real value, everyone else would use it, and we alone would
        // keep the guess. It is a desync with no alarm on it, so put one here.
        // The cause is always the same: sampling and advancing got out of step
        // (see `sample_tick`), which the caller's frame order decides.
        if applied <= self.net.current() {
            self.faults.push(format!(
                "rollback: local input for tick {applied} arrived after that tick was already \
                 simulated — the frame sampled and advanced out of step, and this peer is now \
                 running a tick nobody else is"
            ));
        }
        self.net.add_local(sampled, input)
    }

    /// A remote peer's input for an already-shifted applied tick. A contradiction
    /// with what we simulated is banked and resolved at the top of the next
    /// [`RollbackDriver::advance`] — never mid-frame, because a replay has to
    /// run against a settled world.
    pub fn add_remote(&mut self, peer: PeerId, applied: u64, input: NetInput) -> Option<Correction> {
        let c = self.net.add_remote(peer, applied, input);
        if let Some(c) = c {
            self.pending = Some(self.pending.map_or(c.tick, |p| p.min(c.tick)));
        }
        c
    }

    /// Record a logged input for any peer, the local one included — what a
    /// shadow simulation ([`crate::shadow`]) feeds instead of sampling.
    pub fn feed_logged(&mut self, peer: PeerId, applied: u64, input: NetInput) {
        if let Some(c) = self.net.insert_logged(peer, applied, input) {
            self.pending = Some(self.pending.map_or(c.tick, |p| p.min(c.tick)));
        }
    }

    /// Advance the local simulation by one tick, resolving any banked
    /// correction first. Returns the tick simulated, or `None` when the session
    /// stalled waiting for input (§2.3).
    pub fn advance(&mut self, ctx: &mut Ctx) -> Option<u64> {
        self.resolve_pending(ctx);
        let next = self.net.current() + 1;
        if self.net.should_stall(next) {
            // Degrade into "the game runs slightly slow" rather than "the
            // opponent teleports": beyond the cap the correction's hitch costs
            // more than simply waiting for the input.
            self.stalled = true;
            return None;
        }
        self.stalled = false;
        let saved = self.capture(ctx, next);
        self.ring.push_back(saved);
        self.trim_ring();
        let resolved = self.net.inputs_for(next);
        self.simulate_tick(ctx, next, &resolved);
        Some(next)
    }

    /// Step the simulation BACKWARDS one tick, from the state ring
    /// (`docs/rollback-netcode-design.md` §7 P5 — closes 0024's deferred item).
    ///
    /// Frame-stepping forwards is easy; stepping back is not, because a
    /// simulation is not invertible. It only works here because rollback
    /// already keeps every recent tick's exact state for its own reasons —
    /// backwards stepping is that ring, read by a human instead of by a
    /// correction. That is also its limit: it reaches back exactly as far as
    /// the ring does, and no further.
    ///
    /// Returns the tick now standing, or `None` when the ring can't reach back
    /// any further.
    pub fn step_back(&mut self, ctx: &mut Ctx) -> Option<u64> {
        let current = self.net.current();
        if current == 0 {
            return None;
        }
        let saved = self.ring.iter().find(|s| s.tick == current)?.clone();
        self.apply(ctx, &saved);
        self.ring.retain(|s| s.tick < current);
        self.net.rewind_to(current - 1);
        Some(current - 1)
    }

    /// Restore the last state before the earliest invalidated tick and
    /// re-simulate to the present, with no rendering in between.
    fn resolve_pending(&mut self, ctx: &mut Ctx) {
        let Some(from) = self.pending.take() else { return };
        let to = self.net.current();
        if from > to || self.nodes.is_empty() {
            return;
        }
        let Some(i) = self.ring.iter().position(|s| s.tick == from) else {
            // Past the ring there is no state to restore, so there is no honest
            // way to apply the correction. `should_stall` exists to make this
            // unreachable; if it happens the session is desynced and saying so
            // beats playing out two different matches.
            self.faults.push(format!(
                "rollback: a correction reached back to tick {from} but the state ring only \
                 holds {} tick(s) from {:?} — the simulation may have desynced",
                self.ring.len(),
                self.ring.front().map(|s| s.tick),
            ));
            return;
        };
        // Everything newer than the anchor is provisional and about to be
        // rewritten; the anchor itself is the state BEFORE `from`, which the
        // correction does not change (only `from`'s inputs did).
        let anchor = self.ring[i].clone();
        self.ring.truncate(i);
        ctx.host.begin_replay();
        self.apply(ctx, &anchor);
        self.ring.push_back(anchor);
        for t in from..=to {
            if t > from {
                let s = self.capture(ctx, t);
                self.ring.push_back(s);
                self.trim_ring();
            }
            // Reuse exactly what the original pass ran wherever a real input
            // still hasn't arrived: re-deriving repeat-last now could pick a
            // different source and make the replay disagree with the pass it is
            // supposed to reproduce — a desync that only appears under loss.
            let resolved = self.net.replay_inputs_for(t);
            self.net.record_replay(t, &resolved);
            self.simulate_tick(ctx, t, &resolved);
        }
        ctx.host.end_replay();
        self.resimulated_ticks += to - from + 1;
    }

    /// Simulate exactly one tick of every rollback node from `resolved`.
    ///
    /// The *only* per-tick path there is: [`RollbackDriver::advance`] runs it
    /// live and the replay loop runs it again. Two paths would have to agree by
    /// coincidence; one path agrees by construction.
    fn simulate_tick(&mut self, ctx: &mut Ctx, tick: u64, resolved: &[ResolvedInput]) {
        // Diagnostics + the deterministic RNG seed, before any hook runs. The
        // draw counter resets with it, which is what makes a replayed tick
        // draw exactly the numbers the live tick drew (§3).
        ctx.host.set_rollback_info(floptle_script::RollbackInfo {
            active: true,
            tick,
            seed: self.seed,
            depth: self.net.last_depth,
            max_depth: self.net.max_depth_seen,
            average_depth: if self.net.corrections == 0 {
                0.0
            } else {
                self.resimulated_ticks as f32 / self.net.corrections as f32
            },
            mispredict_rate: self.net.mispredict_rate(),
            input_delay: self.net.delay(),
            stalled: self.stalled,
        });
        let sys = ctx.host.input_system().clone();
        let action_count = sys.borrow().map().actions.len();
        // Every peer's input lands in its own slot, once, before any hook runs
        // — so input history advances exactly one tick for every player whether
        // or not their fighter's script gets around to reading it.
        let mut aim_by_slot: Vec<Option<[f32; 2]>> = vec![None; self.slots.len()];
        for r in resolved {
            let Some(slot) = self.slot_of(r.peer) else { continue };
            let state = floptle_script::net_to_input(&r.input, action_count);
            sys.borrow_mut().set_tick_state(slot, state);
            if let Some(a) = aim_by_slot.get_mut(slot as usize) {
                *a = floptle_script::net_aim(&r.input);
            }
        }
        // Fresh body state for THIS tick (post the previous tick's physics), so
        // `node.vx/grounded` read what the live tick would have read.
        Self::feed_bodies(ctx);
        Self::lend_world(ctx);
        let t = tick as f32 * ctx.step;
        // BOTH hooks run on the tick clock. `update` is a render-rate pass
        // everywhere else, and a render-rate read is one no replay can
        // reproduce — so for a rollback node it rides the tick exactly as it
        // already does for a `Predicted` one (§2.4).
        //
        // Two passes rather than one interleaved pass: in an ordinary frame
        // every `update` runs before any `fixedUpdate`, and a controller is
        // entitled to rely on that.
        for n in self.nodes.clone() {
            Self::set_aim(ctx, aim_by_slot.get(n.slot as usize).copied().flatten());
            ctx.host.run_frame_for(ctx.world, n.eid, ctx.step, t);
        }
        for n in self.nodes.clone() {
            Self::set_aim(ctx, aim_by_slot.get(n.slot as usize).copied().flatten());
            ctx.host.run_fixed_for(ctx.world, n.eid, ctx.step, t);
        }
        Self::reclaim_world(ctx);
        // Component writes (a controller's friction toggle) reach the body, then
        // each rollback body advances exactly one tick. Non-rollback bodies are
        // untouched here — the caller's own physics step still owns them.
        ctx.sim.sync_dynamic_params(ctx.world);
        for n in self.nodes.clone() {
            ctx.sim.step_body_tick(n.eid, ctx.step);
        }
    }

    /// The raw input snapshot for the node about to run: **neutral**, carrying
    /// only `aim`.
    ///
    /// That is the deliberate consequence of an actions-only wire (§0.5.5): raw
    /// key polls have nothing to replay from, so they read neutral identically
    /// on every peer and in every replay. A rollback controller that still polls
    /// raw keys visibly does nothing instead of silently desyncing.
    fn set_aim(ctx: &mut Ctx, aim: Option<[f32; 2]>) {
        ctx.host.set_input(floptle_script::InputSnapshot { aim, ..Default::default() });
    }

    fn feed_bodies(ctx: &mut Ctx) {
        let mut states = std::collections::HashMap::new();
        for (e, vel, up, grounded, height, pos) in ctx.sim.body_states() {
            states.insert(
                e.index(),
                floptle_script::BodyState {
                    vel: [vel.x, vel.y, vel.z],
                    up: [up.x, up.y, up.z],
                    grounded,
                    height,
                    pos: [pos.x, pos.y, pos.z],
                },
            );
        }
        ctx.host.set_bodies(states);
    }

    /// Lend the colliders and body hulls so a replayed hook can raycast.
    ///
    /// Not optional: a fighter's ground probe returning nil mid-replay corrupts
    /// its grounded/friction logic, and the correction then converges on a
    /// state the other machine never computed.
    fn lend_world(ctx: &mut Ctx) {
        let origin = ctx.sim.world.origin;
        let colliders = std::mem::take(&mut ctx.sim.world.colliders);
        ctx.host.set_colliders(colliders, origin);
        let hulls = ctx.sim.body_hulls(ctx.world);
        ctx.host.set_hulls(hulls);
    }

    fn reclaim_world(ctx: &mut Ctx) {
        ctx.sim.world.colliders = ctx.host.take_colliders();
        for (eid, v) in ctx.host.take_body_changes() {
            ctx.sim.set_body_velocity(eid, Vec3::new(v[0], v[1], v[2]));
        }
        for (eid, h) in ctx.host.take_body_height_changes() {
            ctx.sim.set_body_height(eid, h);
        }
        for (eid, p) in ctx.host.take_body_pos_changes() {
            ctx.sim.set_body_position(eid, floptle_core::math::DVec3::new(p[0], p[1], p[2]));
        }
    }

    /// Capture the state the driver would need to re-simulate `tick`.
    fn capture(&mut self, ctx: &mut Ctx, tick: u64) -> SavedTick {
        let mut bodies = Vec::with_capacity(self.nodes.len());
        let mut transforms = Vec::with_capacity(self.nodes.len());
        let mut scripts = Vec::with_capacity(self.nodes.len());
        for n in &self.nodes {
            bodies.push(ctx.sim.body_snapshot(n.eid));
            transforms.push(ctx.world.get::<Transform>(n.entity).copied());
            scripts.push(ctx.host.snapshot_scripts(n.eid));
        }
        let input = ctx.host.input_system().borrow().snapshot_tick();
        SavedTick { tick, bodies, transforms, scripts, input }
    }

    /// Put the world back the way `saved` found it.
    ///
    /// Poses before scripts: a `restore(s)` that reads its node's position gets
    /// a mirror that is one sync stale either way, but restoring the pose first
    /// at least makes the next hook's read correct.
    fn apply(&mut self, ctx: &mut Ctx, saved: &SavedTick) {
        if !ctx.host.input_system().borrow_mut().restore_tick(&saved.input) {
            // The local player count changed under us, so the snapshot's slots
            // no longer mean what they meant. Applying it anyway would put one
            // player's buffered inputs on another.
            self.faults.push(
                "rollback: the input snapshot no longer fits this session's player count — \
                 the tick could not be fully restored"
                    .into(),
            );
        }
        for (i, n) in self.nodes.iter().enumerate() {
            if let (Some(Some(tr)), Some(slot)) =
                (saved.transforms.get(i), ctx.world.get_mut::<Transform>(n.entity))
            {
                *slot = *tr;
            }
            if let Some(Some(bs)) = saved.bodies.get(i) {
                ctx.sim.restore_body(n.eid, bs);
            }
            if let Some(s) = saved.scripts.get(i) {
                ctx.host.restore_scripts(n.eid, s);
            }
        }
    }

    /// Drop saved ticks the ring no longer has room for.
    ///
    /// One rule, not two. §2.1 *permits* dropping everything at or below the
    /// confirmed frontier, and an earlier cut did — which left a healthy
    /// session holding a single saved tick, since in a healthy session every
    /// tick confirms immediately. The depth cap already bounds the memory, and
    /// the ticks that pruning would have thrown away are exactly what backwards
    /// frame-stepping reads (§7 P5). So the cap is the only rule, and what it
    /// keeps is useful rather than merely permitted.
    fn trim_ring(&mut self) {
        let cap = (self.net.max_depth + RING_MARGIN) as usize;
        while self.ring.len() > cap {
            self.ring.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use floptle_core::math::DVec3;
    use floptle_core::{Name, RigidBody, Replicated, ReplicationMode, Scripts, ScriptInst};
    use floptle_input::{Action, Binding, InputMap, Key, Source};
    use floptle_physics::GravityField;

    use super::*;

    const P1: PeerId = 1;
    const P2: PeerId = 2;
    const STEP: f32 = 1.0 / 60.0;

    /// A deliberately small fighter written to the determinism profile the
    /// design documents (§3.1): integer frame counters, no wall clock, no
    /// unseeded RNG, everything the simulation reads inside `snapshot()`.
    ///
    /// It is small but not toy: it holds a per-frame counter, a velocity it
    /// writes to its body, a cooldown that only ticks while an attack is out,
    /// and — the part that matters — it resolves hits by calling **straight
    /// into the other fighter's script**. Cross-script hit resolution is the
    /// thing `Predicted` cannot carry (§1) and the thing a re-simulation has to
    /// reproduce in the same order on both machines.
    const FIGHTER: &str = "\
me = nil\n\
state = { frame = 0, hp = 100, vx = 0, atk = 0, hits = 0, taken = 0 }\n\
function start(node) me = node end\n\
function fixedUpdate(node, dt)\n\
  local pad = input.player(params.player)\n\
  state.frame = state.frame + 1\n\
  -- Movement: integer-ish, driven purely by held actions.\n\
  local dir = 0\n\
  if pad.action(\"Right\") then dir = dir + 1 end\n\
  if pad.action(\"Left\") then dir = dir - 1 end\n\
  state.vx = dir * 3.0\n\
  node.vx = state.vx\n\
  -- Attack: a fresh press starts a 6-frame move; frame 3 is the active one.\n\
  if state.atk > 0 then state.atk = state.atk - 1 end\n\
  if pad.justPressed(\"Punch\") and state.atk == 0 then state.atk = 6 end\n\
  if state.atk == 3 then\n\
    local other = findScript(\"fighter\", params.player == 1 and 2 or 1)\n\
    if other then\n\
      state.hits = state.hits + 1\n\
      other.receiveAttack(7)\n\
    end\n\
  end\n\
end\n\
function receiveAttack(dmg)\n\
  state.hp = state.hp - dmg\n\
  state.taken = state.taken + 1\n\
end\n\
function snapshot()\n\
  local c = {}\n\
  for k, v in pairs(state) do c[k] = v end\n\
  return c\n\
end\n\
function restore(s)\n\
  for k in pairs(state) do state[k] = nil end\n\
  for k, v in pairs(s) do state[k] = v end\n\
end\n";

    /// `findScript(kind, player)` — the smallest stand-in for the engine's
    /// cross-node lookup that keeps the test independent of scene naming.
    const PRELUDE: &str = "\
function findScript(kind, player)\n\
  local n = find(player == 1 and \"P1\" or \"P2\")\n\
  return n and n:getscript(kind) or nil\n\
end\n";

    fn fighter_map() -> InputMap {
        InputMap {
            actions: vec![
                Action {
                    name: "Left".into(),
                    bindings: vec![Binding::new(Source::Key(Key::KeyA))],
                },
                Action {
                    name: "Right".into(),
                    bindings: vec![Binding::new(Source::Key(Key::KeyD))],
                },
                Action {
                    name: "Punch".into(),
                    bindings: vec![Binding::new(Source::Key(Key::KeyJ))],
                },
            ],
            axes1: vec![],
            axes2: vec![],
            motions: vec![],
            players: 2,
            motion_axis: None,
        }
    }

    /// Action bit indices, matching `fighter_map`.
    const LEFT: u64 = 1 << 0;
    const RIGHT: u64 = 1 << 1;
    const PUNCH: u64 = 1 << 2;

    fn held(actions: u64) -> NetInput {
        NetInput { actions, ..Default::default() }
    }

    fn press(actions: u64) -> NetInput {
        NetInput { actions, just_pressed: actions, ..Default::default() }
    }

    struct Fixture {
        world: World,
        sim: Sim,
        host: ScriptHost,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("floptle_rollback_{tag}"));
            let _ = std::fs::create_dir_all(&dir);
            std::fs::write(dir.join("fighter.lua"), format!("{PRELUDE}{FIGHTER}")).unwrap();
            let mut world = World::default();
            for (i, name) in ["P1", "P2"].iter().enumerate() {
                let e = world.spawn();
                world.insert(e, Name((*name).into()));
                world.insert(
                    e,
                    Transform::from_translation(DVec3::new(i as f64 * 4.0 - 2.0, 1.0, 0.0)),
                );
                world.insert(e, RigidBody { radius: 0.5, gravity: false, ..Default::default() });
                world.insert(
                    e,
                    Replicated { mode: ReplicationMode::Rollback, ..Default::default() },
                );
                world.insert(
                    e,
                    Scripts(vec![ScriptInst {
                        kind: "fighter".into(),
                        enabled: true,
                        params: vec![("player".into(), i as f32 + 1.0)],
                        refs: Vec::new(),
                        strs: Vec::new(),
                    }]),
                );
            }
            let sim = Sim::build(&world, &[], GravityField::uniform(Vec3::ZERO), DVec3::ZERO);
            let mut host = ScriptHost::new();
            host.set_input_map(fighter_map());
            // One frame pass to build the instances; the driver's own passes
            // fire `start` from there on.
            host.run(&mut world, &dir, STEP, 0.0);
            Self { world, sim, host }
        }

        fn ctx(&mut self) -> Ctx<'_> {
            Ctx {
                world: &mut self.world,
                sim: &mut self.sim,
                host: &mut self.host,
                step: STEP,
            }
        }

        /// Everything a desync would show up in: both fighters' bodies and both
        /// scripts' rollback state, compared by exact bits.
        fn fingerprint(&mut self, driver: &RollbackDriver) -> String {
            let mut out = String::new();
            for n in driver.nodes() {
                let b = self.sim.body_snapshot(n.eid).expect("body");
                let tr = self.world.get::<Transform>(n.entity).copied().expect("transform");
                out.push_str(&format!(
                    "#{} pos={:016x},{:016x},{:016x} vel={:08x},{:08x},{:08x} g={} ",
                    n.eid,
                    b.pos.x.to_bits(),
                    b.pos.y.to_bits(),
                    b.pos.z.to_bits(),
                    b.vel.x.to_bits(),
                    b.vel.y.to_bits(),
                    b.vel.z.to_bits(),
                    b.grounded,
                ));
                out.push_str(&format!("tr={:016x} ", tr.translation.x.to_bits()));
                out.push_str(&format!("{:?}\n", canonical(&self.host.snapshot_scripts(n.eid))));
            }
            out
        }
    }

    /// Script state with its tables sorted — Lua's `pairs()` order is not
    /// deterministic, so an unsorted comparison would false-alarm between two
    /// runs that agree perfectly. (The wire checksum needs the same treatment;
    /// see §6.)
    fn canonical(s: &ScriptState) -> Vec<(String, String)> {
        fn fmt(v: &floptle_net::NetValue) -> String {
            match v {
                floptle_net::NetValue::Table(pairs) => {
                    let mut kv: Vec<String> =
                        pairs.iter().map(|(k, val)| format!("{}={}", fmt(k), fmt(val))).collect();
                    kv.sort();
                    format!("{{{}}}", kv.join(","))
                }
                floptle_net::NetValue::Num(n) => format!("{:016x}", n.to_bits()),
                other => format!("{other:?}"),
            }
        }
        s.entries.iter().map(|(k, v)| (k.clone(), fmt(v))).collect()
    }

    /// A scripted match: `(tick, P1 input, P2 input)`, chosen so both fighters
    /// move, attack, trade hits and cool down inside the window.
    fn script_of_the_match() -> Vec<(u64, NetInput, NetInput)> {
        let mut v = Vec::new();
        for t in 1..=20u64 {
            let p1 = match t {
                1..=3 => held(RIGHT),
                4 => press(PUNCH | RIGHT),
                5..=8 => held(RIGHT),
                9 => press(PUNCH),
                10..=14 => held(LEFT),
                _ => held(0),
            };
            let p2 = match t {
                1..=5 => held(LEFT),
                6 => press(PUNCH | LEFT),
                7..=11 => held(0),
                12 => press(PUNCH),
                _ => held(RIGHT),
            };
            v.push((t, p1, p2));
        }
        v
    }

    /// **The** correctness test of the whole feature, and it needs no second
    /// machine (§7 P3).
    ///
    /// Two runs of the same twenty-tick match. The first never guesses: every
    /// input is known before its tick. The second is fed P2's inputs LATE for a
    /// stretch in the middle, so the driver predicts them (repeat-last),
    /// simulates several ticks on the guess, then gets contradicted and has to
    /// restore and re-simulate the whole span.
    ///
    /// The two must end BIT-IDENTICAL — same body positions, same velocities,
    /// same script state down to the float bits. Anything less than bit
    /// equality is a rollback implementation that plays a subtly different match
    /// on each screen until someone notices the health bars disagree.
    #[test]
    fn a_re_simulated_span_bit_matches_an_uninterrupted_run() {
        let script = script_of_the_match();

        // --- the uninterrupted run -----------------------------------------
        let mut clean = Fixture::new("clean");
        let mut a = RollbackDriver::new(P1, vec![P1, P2], 0, 0);
        a.rebind(&clean.world, &mut clean.sim, &clean.host);
        for (t, p1, p2) in &script {
            a.add_local(*t, p1.clone());
            a.add_remote(P2, *t, p2.clone());
            assert_eq!(a.advance(&mut clean.ctx()), Some(*t));
        }
        assert_eq!(a.net.corrections, 0, "nothing was ever guessed");
        let expected = clean.fingerprint(&a);

        // --- the mispredicted run ------------------------------------------
        let mut rolled = Fixture::new("rolled");
        let mut b = RollbackDriver::new(P1, vec![P1, P2], 0, 0);
        b.rebind(&rolled.world, &mut rolled.sim, &rolled.host);
        // P2's inputs for ticks 5..=9 are withheld and delivered only after tick
        // 9 has been simulated — right across the tick where P2 throws a punch,
        // so the guess is wrong about a hit landing, not just about a direction.
        let late: Vec<(u64, NetInput)> =
            script.iter().filter(|(t, ..)| (5..=9).contains(t)).map(|(t, _, p2)| (*t, p2.clone())).collect();
        for (t, p1, p2) in &script {
            b.add_local(*t, p1.clone());
            if !(5..=9).contains(t) {
                b.add_remote(P2, *t, p2.clone());
            }
            assert_eq!(b.advance(&mut rolled.ctx()), Some(*t));
            if *t == 9 {
                for (lt, inp) in &late {
                    b.add_remote(P2, *lt, inp.clone());
                }
            }
        }
        assert!(b.net.corrections > 0, "the withheld inputs must have forced a correction");
        assert!(b.resimulated_ticks > 0, "…and the driver must actually have re-simulated");

        assert_eq!(
            rolled.fingerprint(&b),
            expected,
            "a re-simulated span must reproduce the uninterrupted run bit for bit"
        );
        assert!(rolled.host.errors().is_empty(), "errors: {:?}", rolled.host.errors());
        assert!(b.faults.is_empty(), "faults: {:?}", b.faults);
    }

    /// The same scenario driven twice through the same span: a SECOND
    /// correction inside an already-replayed range must land on the replay's
    /// state, not on the original pass's.
    ///
    /// This is where a snapshot that shared its tables with the live sim breaks
    /// — the first replay mutates it and every replay after it starts from
    /// garbage. It only ever shows up under packet loss, which is exactly why it
    /// is tested here and not left to a play session.
    #[test]
    fn two_corrections_over_the_same_span_still_land_on_the_clean_result() {
        let script = script_of_the_match();
        let mut clean = Fixture::new("clean2");
        let mut a = RollbackDriver::new(P1, vec![P1, P2], 0, 0);
        a.rebind(&clean.world, &mut clean.sim, &clean.host);
        for (t, p1, p2) in &script {
            a.add_local(*t, p1.clone());
            a.add_remote(P2, *t, p2.clone());
            a.advance(&mut clean.ctx());
        }
        let expected = clean.fingerprint(&a);

        let mut rolled = Fixture::new("rolled2");
        let mut b = RollbackDriver::new(P1, vec![P1, P2], 0, 0);
        b.rebind(&rolled.world, &mut rolled.sim, &rolled.host);
        let mut ever_stalled = false;
        for (t, p1, p2) in &script {
            b.add_local(*t, p1.clone());
            if !(6..=12).contains(t) {
                b.add_remote(P2, *t, p2.clone());
            }
            // A seven-tick gap outruns the depth cap, so the sim stalls partway
            // and falls behind the wall clock — which is the designed
            // degradation, not a failure. The end state must still be right.
            ever_stalled |= b.advance(&mut rolled.ctx()).is_none();
            // Two late deliveries over overlapping ranges: the second reaches
            // back INTO the span the first already replayed.
            if *t == 10 {
                for (lt, _, p2) in script.iter().filter(|(t, ..)| (9..=10).contains(t)) {
                    b.add_remote(P2, *lt, p2.clone());
                }
            }
            if *t == 14 {
                for (lt, _, p2) in script.iter().filter(|(t, ..)| (6..=12).contains(t)) {
                    b.add_remote(P2, *lt, p2.clone());
                }
            }
        }
        assert!(ever_stalled, "the gap must have outrun the depth cap");
        // Everything has arrived; the stalled sim catches up to the same tick.
        while b.net.current() < script.len() as u64 {
            assert!(b.advance(&mut rolled.ctx()).is_some(), "nothing is outstanding any more");
        }
        assert!(b.net.corrections >= 2, "both deliveries must have corrected");
        assert_eq!(rolled.fingerprint(&b), expected, "two replays must converge on the same state");
    }

    /// Past the depth cap the driver waits instead of guessing further (§2.3):
    /// A link slow enough to stall the session repeatedly, with a pad whose
    /// value depends on WHEN it was polled rather than on which tick it feeds.
    ///
    /// This is the shape of a real player's hands, and it is the case a
    /// scripted match cannot test: a scripted input is the same value however
    /// many times you ask for it, so re-sampling a tick is invisible. Poll a
    /// changing pad twice for one tick and it is anything but — the second
    /// value overwrites the first in our own history while the fan-out's
    /// per-`(peer, tick)` dedup drops it on the way out, and the two machines
    /// simulate that tick from different inputs. [`RollbackDriver::sample_tick`]
    /// is what stops it, so the test drives sampling through exactly that
    /// predicate rather than a copy of it.
    #[test]
    fn a_changing_pad_polled_across_a_stall_still_leaves_the_peers_agreeing() {
        use floptle_net::{MemoryHub, NetSession, SERVER};

        let hub = MemoryHub::new();
        let mut host_net = NetSession::server(Box::new(hub.server_endpoint()), 0);
        let mut peer_net = NetSession::client(Box::new(hub.connect()), 0);
        let (hw, mut pw) = (World::default(), World::default());
        let mut wall = 0u64;
        let mut pump = |n: u64,
                        wall: &mut u64,
                        host_net: &mut NetSession,
                        peer_net: &mut NetSession| {
            for _ in 0..n {
                hub.set_now(*wall);
                host_net.tick_server(&hw, *wall);
                peer_net.tick_client(&mut pw);
                *wall += 1;
            }
        };
        pump(4, &mut wall, &mut host_net, &mut peer_net);
        host_net.set_rollback(true, 2, 0x5EED_1234_ABCD_0001);
        pump(4, &mut wall, &mut host_net, &mut peer_net);
        let (roster, delay, seed) =
            peer_net.take_rollback_start().expect("the host announces the match");
        // Only NOW does the link go bad: far past what delay 2 and a depth cap
        // of 8 can absorb, so the session spends most of its life waiting —
        // which is the point.
        hub.set_conditions(14, 0.15);

        let mut host = Fixture::new("stall_host");
        let mut peer = Fixture::new("stall_peer");
        let mut host_d = RollbackDriver::new(SERVER, roster.clone(), delay, seed);
        let mut peer_d = RollbackDriver::new(1, roster, delay, seed);
        host_d.rebind(&host.world, &mut host.sim, &host.host);
        peer_d.rebind(&peer.world, &mut peer.sim, &peer.host);
        for (applied, input) in host_d.net.prime_warmup() {
            host_net.push_rollback_input(applied, input);
        }
        for (applied, input) in peer_d.net.prime_warmup() {
            peer_net.send_rollback_input(applied, input);
        }

        // The pad reads differently on every frame, and differently per side.
        let pad = |frame: u64, side: u64| match (frame + side * 3) % 5 {
            0 => held(RIGHT),
            1 => press(PUNCH),
            2 => held(LEFT),
            3 => held(RIGHT | LEFT),
            _ => held(0),
        };
        let target = 24u64;
        let mut stalls = 0u32;
        // Deliberately `net_rollback_tick`'s exact order — pump, drain, sample,
        // advance. Sampling before the drain would decide "stalled" against a
        // frontier the very next line moves, and a frame that skips its sample
        // but still advances leaves an applied tick with no local input in it
        // forever. That ordering is load-bearing, so the test carries it.
        for _ in 0..(target * 20) {
            hub.set_now(wall);
            host_net.tick_server(&hw, wall);
            peer_net.tick_client(&mut pw);
            wall += 1;
            for (host_side, driver) in [(true, &mut host_d), (false, &mut peer_d)] {
                let incoming = if host_side {
                    host_net.take_rollback_inputs()
                } else {
                    peer_net.take_rollback_inputs()
                };
                let local = driver.net.local();
                for (p, applied, input) in incoming {
                    if p != local {
                        driver.add_remote(p, applied, input);
                    }
                }
            }
            for (side, driver) in [(0u64, &mut host_d), (1u64, &mut peer_d)] {
                // THE contract under test: ask the driver whether this frame
                // may sample at all, and poll the pad only if it says yes.
                let Some(sampled) = driver.sample_tick() else { continue };
                let ni = pad(wall, side);
                let applied = driver.add_local(sampled, ni.clone());
                if side == 0 {
                    host_net.push_rollback_input(applied, ni);
                } else {
                    peer_net.send_rollback_input(applied, ni);
                }
            }
            host_d.advance(&mut host.ctx());
            peer_d.advance(&mut peer.ctx());
            // Counted off the DRIVER's own flag, not off `sample_tick` — the
            // point is that the link stalled, not that we declined to sample.
            stalls += u32::from(host_d.stalled) + u32::from(peer_d.stalled);
            if host_d.net.confirmed() >= target && peer_d.net.confirmed() >= target {
                break;
            }
        }
        assert!(
            stalls > 0,
            "the link was supposed to be slow enough to stall — this test proves nothing without it"
        );
        assert!(
            host_d.net.confirmed() >= target && peer_d.net.confirmed() >= target,
            "the match never confirmed {target} ticks (host {}, peer {})",
            host_d.net.confirmed(),
            peer_d.net.confirmed(),
        );
        // Compared at a CONFIRMED tick, not at the live frontier. On a link this
        // slow the newest few ticks on either machine are still provisional —
        // simulated from guesses whose real inputs are literally still in the
        // air — so two peers disagreeing there means nothing. Tick `target` has
        // every peer's real input in it and will never be re-simulated again;
        // if the machines disagree about THAT, they disagree about the match.
        let (h, p) = (host_d.state_hash(target), peer_d.state_hash(target));
        assert!(h.is_some(), "tick {target} fell off the host's ring before it could be compared");
        assert_eq!(
            h, p,
            "a tick re-sampled across a stall made the two machines simulate different inputs"
        );
        assert!(host_d.faults.is_empty(), "host faults: {:?}", host_d.faults);
        assert!(peer_d.faults.is_empty(), "peer faults: {:?}", peer_d.faults);
    }

    /// the game degrades into "runs slightly slow", never into "the opponent
    /// teleports".
    #[test]
    fn past_the_depth_cap_the_driver_stalls_instead_of_guessing() {
        let mut f = Fixture::new("stall");
        let mut d = RollbackDriver::new(P1, vec![P1, P2], 0, 0);
        d.net.max_depth = 4;
        d.rebind(&f.world, &mut f.sim, &f.host);
        // Only P1 is ever heard from, so the confirmed frontier never moves.
        for t in 1..=4u64 {
            d.add_local(t, held(RIGHT));
            assert_eq!(d.advance(&mut f.ctx()), Some(t), "within the cap we predict and go");
        }
        d.add_local(5, held(RIGHT));
        assert_eq!(d.advance(&mut f.ctx()), None, "past it we wait");
        assert!(d.stalled);
        // P2 finally speaks: the frontier moves and the sim resumes.
        for t in 1..=5u64 {
            d.add_remote(P2, t, held(0));
        }
        assert_eq!(d.advance(&mut f.ctx()), Some(5));
        assert!(!d.stalled);
    }

    /// The ring is bounded, and bounded by the cap the stall predicate enforces
    /// — so "the correction reached past the ring" stays unreachable rather
    /// than becoming a rare desync.
    #[test]
    fn the_state_ring_stays_bounded_and_always_covers_the_depth_cap() {
        let mut f = Fixture::new("ring");
        let mut d = RollbackDriver::new(P1, vec![P1, P2], 0, 0);
        d.rebind(&f.world, &mut f.sim, &f.host);
        for t in 1..=60u64 {
            d.add_local(t, held(RIGHT));
            // P2 lags a long way behind, so nothing confirms and the ring is
            // held at its cap by the margin rule rather than by the frontier.
            if t > 10 {
                d.add_remote(P2, t - 10, held(0));
            }
            d.advance(&mut f.ctx());
        }
        let cap = (d.net.max_depth + RING_MARGIN) as usize;
        assert!(d.ring_depth() <= cap, "ring grew to {} (cap {cap})", d.ring_depth());
        assert!(
            d.ring_depth() > d.net.max_depth as usize,
            "…and it must still reach back further than a correction can"
        );
        assert!(d.ring_bytes() > 0, "the memory readout must have something to report");

        // Releasing hands the bodies back to the whole-world step and drops the
        // ring — Stop, or the session ending, must not leave the physics sim
        // waiting for a driver that is gone.
        d.release(&mut f.sim);
        assert_eq!(d.ring_depth(), 0);
        assert!(d.nodes().is_empty());
    }

    /// Two real peers, two real simulations, one lossy simulated link — and one
    /// match (§7 P4).
    ///
    /// This is the driver and the wire together: each peer samples only its own
    /// pad, ships it to the other through `NetSession` over a `MemoryHub` with
    /// four ticks of one-way latency and 30% packet loss, predicts what it
    /// hasn't heard, and rolls back when it turns out wrong. Nothing about a hit
    /// crosses the wire — only inputs do — so if the two simulations agree at
    /// the end, they agreed about every hit along the way.
    ///
    /// Packet loss is the point of running it here rather than on a clean link:
    /// it is what makes the driver re-simulate ticks it has already re-simulated
    /// once, which is where reusing the ORIGINAL guess for still-missing inputs
    /// stops being a nicety.
    #[test]
    fn two_peers_over_a_lossy_link_simulate_the_same_match() {
        use floptle_net::{MemoryHub, NetSession, SERVER};

        let hub = MemoryHub::new();
        hub.set_conditions(4, 0.3);
        let mut host_net = NetSession::server(Box::new(hub.server_endpoint()), 0);
        let mut peer_net = NetSession::client(Box::new(hub.connect()), 0);
        // The sessions carry nothing but inputs here, so their replication
        // worlds stay empty — which is the design's claim made literal.
        let (hw, mut pw) = (World::default(), World::default());
        let mut wall = 0u64;
        let mut pump = |n: u64,
                        wall: &mut u64,
                        host_net: &mut NetSession,
                        peer_net: &mut NetSession| {
            for _ in 0..n {
                hub.set_now(*wall);
                host_net.tick_server(&hw, *wall);
                peer_net.tick_client(&mut pw);
                *wall += 1;
            }
        };
        pump(4, &mut wall, &mut host_net, &mut peer_net);
        host_net.set_rollback(true, 2, 0x0BAD_F00D_1234_5678);
        pump(8, &mut wall, &mut host_net, &mut peer_net);
        let (roster, delay, seed) =
            peer_net.take_rollback_start().expect("the host announces the match");
        assert_eq!(roster, vec![SERVER, 1]);

        let mut host = Fixture::new("link_host");
        let mut peer = Fixture::new("link_peer");
        let mut host_d = RollbackDriver::new(SERVER, roster.clone(), delay, seed);
        let mut peer_d = RollbackDriver::new(1, roster, delay, seed);
        host_d.rebind(&host.world, &mut host.sim, &host.host);
        peer_d.rebind(&peer.world, &mut peer.sim, &peer.host);
        for (applied, input) in host_d.net.prime_warmup() {
            host_net.push_rollback_input(applied, input);
        }
        for (applied, input) in peer_d.net.prime_warmup() {
            peer_net.send_rollback_input(applied, input);
        }

        let script = script_of_the_match();
        let input_for = |driver_tick: u64, host_side: bool| {
            script
                .iter()
                .find(|(t, ..)| *t == driver_tick)
                .map(|(_, p1, p2)| if host_side { p1.clone() } else { p2.clone() })
                .unwrap_or_else(|| held(0))
        };
        let target = script.len() as u64;
        // Generous wall time: latency and loss both cost the peers ticks, and a
        // stall is a legitimate outcome of either.
        for _ in 0..(target * 4) {
            for (peer_side, driver) in [(false, &mut host_d), (true, &mut peer_d)] {
                let sampled = driver.net.current() + 1;
                if sampled > target {
                    continue;
                }
                let ni = input_for(sampled, !peer_side);
                let applied = driver.add_local(sampled, ni.clone());
                if peer_side {
                    peer_net.send_rollback_input(applied, ni);
                } else {
                    host_net.push_rollback_input(applied, ni);
                }
            }
            hub.set_now(wall);
            host_net.tick_server(&hw, wall);
            peer_net.tick_client(&mut pw);
            wall += 1;
            for (peer_side, driver) in [(false, &mut host_d), (true, &mut peer_d)] {
                let incoming = if peer_side {
                    peer_net.take_rollback_inputs()
                } else {
                    host_net.take_rollback_inputs()
                };
                let local = driver.net.local();
                for (p, applied, input) in incoming {
                    if p != local {
                        driver.add_remote(p, applied, input);
                    }
                }
            }
            if host_d.net.current() < target {
                host_d.advance(&mut host.ctx());
            }
            if peer_d.net.current() < target {
                peer_d.advance(&mut peer.ctx());
            }
            if host_d.net.current() >= target && peer_d.net.current() >= target {
                break;
            }
        }
        assert_eq!(host_d.net.current(), target, "the host never finished the match");
        assert_eq!(peer_d.net.current(), target, "the peer never finished the match");
        assert!(
            host_d.net.corrections > 0 || peer_d.net.corrections > 0,
            "a lossy 4-tick link must have produced at least one mispredict"
        );
        assert_eq!(
            host.fingerprint(&host_d),
            peer.fingerprint(&peer_d),
            "two peers fed the same inputs must have simulated the same match"
        );
        assert!(host_d.faults.is_empty(), "host faults: {:?}", host_d.faults);
        assert!(peer_d.faults.is_empty(), "peer faults: {:?}", peer_d.faults);
        assert!(host.host.errors().is_empty(), "host script errors: {:?}", host.host.errors());
    }

    /// The checksum both peers publish (§6) must agree when the simulations do,
    /// and disagree the moment they don't. A checksum that can't tell the
    /// difference is decoration on the one mechanism that has to be trustworthy.
    #[test]
    fn the_state_checksum_agrees_between_peers_and_catches_a_divergence() {
        let script = script_of_the_match();
        let run = |tag: &str, tamper: bool| -> (u64, u64) {
            let mut f = Fixture::new(tag);
            let mut d = RollbackDriver::new(P1, vec![P1, P2], 0, 0);
            d.rebind(&f.world, &mut f.sim, &f.host);
            let mut last = 0;
            for tick in 1..=40u64 {
                let (p1, p2) = script
                    .iter()
                    .find(|(t, ..)| *t == tick)
                    .map(|(_, a, b)| (a.clone(), b.clone()))
                    .unwrap_or((held(0), held(0)));
                // One nudge, on one machine, on one tick — the shape a real
                // desync takes.
                let p2 = if tamper && tick == 12 { held(RIGHT) } else { p2 };
                d.add_local(tick, p1);
                d.add_remote(P2, tick, p2);
                d.advance(&mut f.ctx());
                if let Some((t, h)) = d.due_checksum() {
                    last = h;
                    assert_eq!(t, 30, "the checksum tick is derived, not negotiated");
                }
            }
            (last, d.net.confirmed())
        };
        let (clean, confirmed) = run("hash_a", false);
        let (same, _) = run("hash_b", false);
        let (drifted, _) = run("hash_c", true);
        assert!(confirmed >= floptle_net::CHECKSUM_EVERY, "a checksum must have come due");
        assert_ne!(clean, 0);
        assert_eq!(clean, same, "two agreeing runs must publish the same checksum");
        assert_ne!(clean, drifted, "one tick of divergence must change it");
    }

    /// Backwards frame-step (§7 P5): a fighting game is authored in single
    /// frames, and "was that jab active on 4 or on 5" is a question you answer
    /// by stepping back to look — which is only possible because rollback
    /// already keeps the exact state of every recent tick.
    ///
    /// Stepping back and forward again must land on the state that was there
    /// before, or it is a fancy undo rather than a frame-step.
    #[test]
    fn stepping_back_and_forward_again_lands_on_the_same_state() {
        let script = script_of_the_match();
        let mut f = Fixture::new("stepback");
        let mut d = RollbackDriver::new(P1, vec![P1, P2], 0, 0);
        d.rebind(&f.world, &mut f.sim, &f.host);
        for (t, p1, p2) in &script {
            d.add_local(*t, p1.clone());
            d.add_remote(P2, *t, p2.clone());
            d.advance(&mut f.ctx());
        }
        let at_end = f.fingerprint(&d);
        let end_tick = d.net.current();

        // Back four frames — far enough to cross the punch on tick 12.
        for back in 1..=4u64 {
            assert_eq!(d.step_back(&mut f.ctx()), Some(end_tick - back), "step {back} back");
        }
        let stepped_back = f.fingerprint(&d);
        assert_ne!(stepped_back, at_end, "the world must actually have moved back");

        // …and forward again, on the same inputs, to the same place.
        for _ in 0..4 {
            assert!(d.advance(&mut f.ctx()).is_some());
        }
        assert_eq!(d.net.current(), end_tick);
        assert_eq!(
            f.fingerprint(&d),
            at_end,
            "stepping back and forward again must reproduce the state exactly"
        );

        // The ring is the limit, and reaching it is a clean stop rather than a
        // wrong answer.
        let mut steps = 0;
        while d.step_back(&mut f.ctx()).is_some() {
            steps += 1;
            assert!(steps < 100, "step_back must terminate");
        }
        assert!(steps > 0, "at least one step back must have been possible");
    }

    /// A pushbox-only body integrates its velocity and nothing else — no
    /// gravity, no depenetration, no ground detection (§3, §8). The script owns
    /// where the fighter is allowed to be, which is both the deterministic
    /// profile and how the genre actually works.
    #[test]
    fn a_pushbox_only_body_integrates_but_is_never_solved() {
        use floptle_physics::GravityField;

        let mut world = World::default();
        let mut ents = Vec::new();
        for (i, pushbox) in [(0usize, false), (1, true)] {
            let e = world.spawn();
            world.insert(e, Transform::from_translation(DVec3::new(i as f64 * 4.0, 5.0, 0.0)));
            world.insert(
                e,
                RigidBody { radius: 0.5, pushbox_only: pushbox, ..Default::default() },
            );
            ents.push(e);
        }
        let mut sim = Sim::build(
            &world,
            &[],
            GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)),
            DVec3::ZERO,
        );
        sim.set_body_velocity(ents[1].index(), Vec3::new(3.0, 0.0, 0.0));
        for _ in 0..60 {
            sim.step_tick(1.0 / 60.0, None);
        }
        let solved = sim.body_snapshot(ents[0].index()).unwrap();
        let pushbox = sim.body_snapshot(ents[1].index()).unwrap();
        assert!(solved.pos.y < 4.0, "the ordinary body falls, got {}", solved.pos.y);
        assert_eq!(pushbox.pos.y, 5.0, "the pushbox does NOT: gravity is the script's job");
        assert!(
            (pushbox.pos.x - (4.0 + 3.0)).abs() < 1e-4,
            "…but it still moves under its own velocity, got {}",
            pushbox.pos.x
        );
        assert!(!pushbox.grounded, "and it is never told it landed on anything");
        // It is still a box you can hit — that is the entire point of the name.
        assert!(
            sim.body_hulls(&world).iter().any(|h| h.eid == ents[1].index()),
            "a pushbox must stay visible to raycasts and overlap queries"
        );
    }

    /// A `Rollback` node whose scripts define neither hook is almost always a
    /// mistake. The driver says so once, at bind time, rather than desyncing
    /// quietly halfway through a match.
    #[test]
    fn a_rollback_node_without_hooks_is_reported_not_silently_accepted() {
        let dir = std::env::temp_dir().join("floptle_rollback_nohooks");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("cosmetic.lua"), "function fixedUpdate(node, dt) end\n").unwrap();
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Name("Spinner".into()));
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Replicated { mode: ReplicationMode::Rollback, ..Default::default() });
        world.insert(
            e,
            Scripts(vec![ScriptInst {
                kind: "cosmetic".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut sim = Sim::build(&world, &[], GravityField::uniform(Vec3::ZERO), DVec3::ZERO);
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, STEP, 0.0);
        let mut d = RollbackDriver::new(P1, vec![P1], 0, 0);
        d.rebind(&world, &mut sim, &host);
        assert_eq!(d.nodes().len(), 1);
        assert!(
            d.faults.iter().any(|f| f.contains("Spinner") && f.contains("snapshot()")),
            "the warning must name the node and the missing hooks: {:?}",
            d.faults
        );
    }
}
