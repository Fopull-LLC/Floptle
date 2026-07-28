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

// Phase 3 lands the driver and its acceptance test; phase 4 is what puts a
// session behind it and calls into here from the play loop. Until then nothing
// in the editor constructs one, and the compiler is right to say so — remove
// this the moment `net.rs` engages the driver.
#![allow(dead_code)]

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
    /// Diagnostics: ticks re-simulated since the session started. Divided by
    /// `net.corrections` it is the average rollback depth a player actually
    /// felt.
    pub resimulated_ticks: u64,
    /// Problems that must reach the Console rather than being swallowed — a
    /// correction reaching past the ring, an input snapshot that no longer
    /// fits, a `Rollback` node with no rollback hooks. Drained by the driver's
    /// owner.
    pub faults: Vec<String>,
}

impl RollbackDriver {
    /// A session where `peers` (including `local`) play in slot order, applying
    /// local input `delay` ticks after it is sampled.
    pub fn new(local: PeerId, peers: Vec<PeerId>, delay: u8) -> Self {
        Self {
            slots: peers.clone(),
            net: Rollback::new(local, peers, delay),
            nodes: Vec::new(),
            ring: VecDeque::new(),
            pending: None,
            stalled: false,
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
        sim.set_driven_bodies(&self.eids());
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

    /// Record the local player's sampled input. Returns the tick it will
    /// APPLY on (`sampled + delay`) — which is what goes on the wire, so peers
    /// never have to know our delay.
    pub fn add_local(&mut self, sampled: u64, input: NetInput) -> u64 {
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
        for (e, vel, up, grounded, height) in ctx.sim.body_states() {
            states.insert(
                e.index(),
                floptle_script::BodyState {
                    vel: [vel.x, vel.y, vel.z],
                    up: [up.x, up.y, up.z],
                    grounded,
                    height,
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

    /// Drop saved ticks nothing can reach any more (§2.1).
    fn trim_ring(&mut self) {
        let cap = (self.net.max_depth + RING_MARGIN) as usize;
        while self.ring.len() > cap {
            self.ring.pop_front();
        }
        // Everything at or below the confirmed frontier is settled and can never
        // be re-simulated — except the newest one, which stays as the checksum
        // anchor (§6).
        let confirmed = self.net.confirmed();
        while self.ring.len() > 1 && self.ring[1].tick <= confirmed {
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
        let mut a = RollbackDriver::new(P1, vec![P1, P2], 0);
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
        let mut b = RollbackDriver::new(P1, vec![P1, P2], 0);
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
        let mut a = RollbackDriver::new(P1, vec![P1, P2], 0);
        a.rebind(&clean.world, &mut clean.sim, &clean.host);
        for (t, p1, p2) in &script {
            a.add_local(*t, p1.clone());
            a.add_remote(P2, *t, p2.clone());
            a.advance(&mut clean.ctx());
        }
        let expected = clean.fingerprint(&a);

        let mut rolled = Fixture::new("rolled2");
        let mut b = RollbackDriver::new(P1, vec![P1, P2], 0);
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
    /// the game degrades into "runs slightly slow", never into "the opponent
    /// teleports".
    #[test]
    fn past_the_depth_cap_the_driver_stalls_instead_of_guessing() {
        let mut f = Fixture::new("stall");
        let mut d = RollbackDriver::new(P1, vec![P1, P2], 0);
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
        let mut d = RollbackDriver::new(P1, vec![P1, P2], 0);
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
        let mut d = RollbackDriver::new(P1, vec![P1], 0);
        d.rebind(&world, &mut sim, &host);
        assert_eq!(d.nodes().len(), 1);
        assert!(
            d.faults.iter().any(|f| f.contains("Spinner") && f.contains("snapshot()")),
            "the warning must name the node and the missing hooks: {:?}",
            d.faults
        );
    }
}
