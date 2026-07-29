//! Rollback bookkeeping (`docs/rollback-netcode-design.md`): per-peer input
//! rings, input delay, remote-input prediction, the confirmed frontier, and
//! mispredict detection.
//!
//! Like [`crate::predict`], this is **pure bookkeeping** — it decides *whether*
//! and *how far* to roll back; the driver owns the actual state ring and
//! re-simulation, because that means running `fixedUpdate` and physics bodies,
//! which live outside this crate.
//!
//! The loop a driver runs:
//!
//! 1. Sample local input for tick `T` and [`Rollback::add_local`] it. The queue
//!    applies it at `T + delay` — see [`Rollback::delay`].
//! 2. [`Rollback::inputs_for`] the tick about to simulate. Every peer answers:
//!    a real input where one has arrived, otherwise a **prediction** (repeat the
//!    peer's last known input).
//! 3. Simulate, then save the resulting state keyed by that tick.
//! 4. Remote inputs arrive: [`Rollback::add_remote`]. If one lands for a tick
//!    already simulated **and disagrees with what was predicted**, the returned
//!    [`Correction`] names the tick to restore to; the driver restores that
//!    tick's saved state and re-simulates forward to the current tick with no
//!    rendering in between.
//! 5. [`Rollback::confirmed`] is the newest tick where every peer's real input is
//!    known — the driver may drop saved states at or below it.
//!
//! ## Why the arithmetic is fussy about `delay`
//!
//! Local input sampled on tick `T` is *applied* on `T + delay`. That is the
//! whole point: it buys the network `delay` ticks to deliver before anyone has
//! to guess. Everything in this module speaks in **applied** ticks — the tick a
//! `NetInput` actually drives simulation on — so a driver never has to do the
//! shift itself and get it wrong in one of the two places.

use std::collections::HashMap;

use crate::transport::PeerId;
use crate::wire::NetInput;

/// How many applied ticks of input each peer's ring keeps. Generous next to any
/// playable rollback depth; older entries are confirmed-or-irrelevant.
pub const INPUT_RING: usize = 256;

/// The largest input delay a session may configure — 6 ticks, 100 ms at 60 Hz.
///
/// Two ceilings meet here and the lower one wins. Past roughly this much added
/// latency the delay costs more than the mispredicts it avoids: a fighting
/// game's whole premise is that the button happens when you press it, and 100 ms
/// is already at the edge of what a player reads as "the game", not "the link".
/// It also has to stay inside [`DEFAULT_MAX_DEPTH`]: a delay deeper than the
/// state ring is a delay whose corrections could not be rolled back to.
///
/// Requests above it are clamped, loudly rather than silently — a game asking
/// for 12 has misunderstood something, and finding that out from a log line
/// beats finding it out from a match.
pub const MAX_DELAY: u8 = 6;

/// The FLOOR on input delay, ticks (~33 ms at 60 Hz), and what a LAN session
/// runs at.
///
/// It was the only delay for two releases, which is right for peers in the same
/// building and wrong for everyone else: past 33 ms one way the opponent's
/// input lands after the tick that needed it on every tick, so the driver
/// guesses and re-simulates forever. Correct, and six times the work
/// (floptle/0049). The host now derives a starting value from the worst peer's
/// measured RTT, and a game can name one outright.
///
/// Still fixed per session, never auto-adjusted mid-match: adaptive delay hides
/// a bad connection by changing how the game *feels* while you are playing it,
/// which a fighting game cannot tolerate. The measurement is exposed instead,
/// so a delay is chosen informed and then left alone (§2.2).
pub const DEFAULT_INPUT_DELAY: u8 = 2;

/// The default rollback depth cap. Past roughly this many ticks the correction
/// hitch is more visible than the latency of simply waiting, so the driver
/// stalls instead of re-simulating (see [`Rollback::should_stall`]).
pub const DEFAULT_MAX_DEPTH: u32 = 8;

/// One peer's input for one applied tick, and whether it is real or a guess.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedInput {
    pub peer: PeerId,
    pub input: NetInput,
    /// False when this is a repeat-last prediction rather than the peer's real
    /// input. A tick with any predicted input is provisional.
    pub real: bool,
}

/// What a late-arriving input requires. Returned by [`Rollback::add_remote`]
/// only when the arrival actually contradicts a simulated tick — an input that
/// matches what was predicted costs nothing, which is the common case for a
/// player holding a direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Correction {
    /// The earliest tick whose simulation is now known to be wrong. The driver
    /// restores the state saved *before* this tick and re-simulates from here.
    pub tick: u64,
    /// `current − tick + 1`: how many ticks must be re-simulated.
    pub depth: u32,
}

/// Per-peer input bookkeeping for one rollback session.
pub struct Rollback {
    /// Peers in the session, including the local one. Order is irrelevant to
    /// correctness here; the driver maps peers to fighters.
    peers: Vec<PeerId>,
    local: PeerId,
    /// Ticks between sampling a local input and applying it.
    delay: u8,
    /// `(peer, applied tick) -> input`, real arrivals only.
    real: HashMap<(PeerId, u64), NetInput>,
    /// What we actually simulated with, for every tick we have simulated. Needed
    /// to answer "did the real input contradict the guess?" — comparing against
    /// the peer's last input instead would re-derive the prediction and get it
    /// wrong whenever two guesses in a row differed.
    used: HashMap<(PeerId, u64), NetInput>,
    /// Newest applied tick with a real input, per peer — the repeat-last source.
    newest_real: HashMap<PeerId, u64>,
    /// The newest tick the driver has simulated (0 = nothing yet).
    current: u64,
    /// Newest tick where EVERY peer's real input is known.
    confirmed: u64,
    /// Rollback depth beyond which the driver should stall rather than
    /// re-simulate. See [`Rollback::should_stall`].
    pub max_depth: u32,
    /// Diagnostics — the Hub's multiplayer panel and `net.rollback*` read these.
    pub corrections: u64,
    pub last_depth: u32,
    pub max_depth_seen: u32,
    /// Total (predicted ticks, simulated ticks) — their ratio is the mispredict
    /// rate a player actually cares about.
    pub predicted_ticks: u64,
    pub simulated_ticks: u64,
}

impl Rollback {
    /// A session with `peers` (including `local`) and an input delay in ticks.
    pub fn new(local: PeerId, peers: Vec<PeerId>, delay: u8) -> Self {
        Self {
            peers,
            local,
            delay: delay.min(MAX_DELAY),
            real: HashMap::new(),
            used: HashMap::new(),
            newest_real: HashMap::new(),
            current: 0,
            confirmed: 0,
            max_depth: DEFAULT_MAX_DEPTH,
            corrections: 0,
            last_depth: 0,
            max_depth_seen: 0,
            predicted_ticks: 0,
            simulated_ticks: 0,
        }
    }

    pub fn delay(&self) -> u8 {
        self.delay
    }

    pub fn local(&self) -> PeerId {
        self.local
    }

    pub fn peers(&self) -> &[PeerId] {
        &self.peers
    }

    pub fn confirmed(&self) -> u64 {
        self.confirmed
    }

    pub fn current(&self) -> u64 {
        self.current
    }

    /// A peer joined or left. Their inputs are kept (harmless) but the confirmed
    /// frontier is recomputed, since it depends on WHO must be heard from.
    pub fn set_peers(&mut self, peers: Vec<PeerId>) {
        self.peers = peers;
        self.recompute_confirmed();
    }

    /// The applied tick for input sampled on `sampled`.
    pub fn applied_tick(&self, sampled: u64) -> u64 {
        sampled + self.delay as u64
    }

    /// Record the LOCAL peer's input, sampled on tick `sampled` and therefore
    /// applied on `sampled + delay`. Returns the applied tick, which is what the
    /// driver sends to peers — they must never have to know our delay.
    pub fn add_local(&mut self, sampled: u64, input: NetInput) -> u64 {
        let applied = self.applied_tick(sampled);
        self.insert_real(self.local, applied, input);
        applied
    }

    /// Seed the local peer's real inputs for the session's warm-up ticks, and
    /// return them so the driver can ship them like any other input.
    ///
    /// Input sampled on tick `T` applies on `T + delay`, so ticks `1..=delay`
    /// have no local sample behind them — nothing was pressed before the match
    /// began. Neutral is not a guess there, it is the truth. Leaving them empty
    /// would be: the confirmed frontier could never pass 0, and the session
    /// would stall against the depth cap a few ticks in and never recover.
    pub fn prime_warmup(&mut self) -> Vec<(u64, NetInput)> {
        let mut out = Vec::new();
        for applied in 1..=self.delay as u64 {
            let input = NetInput::default();
            self.insert_real(self.local, applied, input.clone());
            out.push((applied, input));
        }
        out
    }

    /// A remote peer's input for an already-shifted APPLIED tick.
    ///
    /// Returns `Some(Correction)` only when this arrival contradicts a tick that
    /// has already been simulated. Duplicates, inputs for the future, and
    /// arrivals that match the prediction all return `None` — which is the
    /// overwhelming majority of packets and must stay free.
    pub fn add_remote(&mut self, peer: PeerId, applied: u64, input: NetInput) -> Option<Correction> {
        if peer == self.local {
            return None; // our own input echoed back; we are the authority on it
        }
        self.insert_logged(peer, applied, input)
    }

    /// Record a real input for ANY peer — the local one included — at an
    /// already-shifted applied tick.
    ///
    /// What a replay and the referee do: every peer's input is already in the
    /// log, including whoever this instance stands in for, so nothing is ever
    /// sampled and nothing needs the delay applied to it a second time.
    /// [`Self::add_local`] would shift it again and play the match a couple of
    /// ticks skewed; [`Self::add_remote`] would silently ignore it.
    pub fn insert_logged(
        &mut self,
        peer: PeerId,
        applied: u64,
        input: NetInput,
    ) -> Option<Correction> {
        // A duplicate (the redundant window resends the last few ticks every
        // packet) must not re-trigger anything.
        if self.real.get(&(peer, applied)).is_some_and(|prev| *prev == input) {
            return None;
        }
        let contradicts = applied <= self.current
            && self.used.get(&(peer, applied)).is_some_and(|guess| *guess != input);
        self.insert_real(peer, applied, input);
        if !contradicts {
            return None;
        }
        // Everything from this tick forward was simulated against a guess we now
        // know was wrong.
        let depth = (self.current - applied + 1) as u32;
        self.corrections += 1;
        self.last_depth = depth;
        self.max_depth_seen = self.max_depth_seen.max(depth);
        Some(Correction { tick: applied, depth })
    }

    fn insert_real(&mut self, peer: PeerId, applied: u64, input: NetInput) {
        self.real.insert((peer, applied), input);
        let newest = self.newest_real.entry(peer).or_insert(applied);
        *newest = (*newest).max(applied);
        self.recompute_confirmed();
        self.prune();
    }

    /// Move the simulated frontier back to `tick` — the editor's BACKWARDS
    /// frame-step, which reads the driver's state ring instead of re-deriving
    /// anything (§7 P5).
    ///
    /// Only the frontier moves; the input rings are untouched, so stepping
    /// forward again re-simulates from exactly the same commands and lands on
    /// exactly the same state. That is what makes it a *step*, not an undo.
    pub fn rewind_to(&mut self, tick: u64) {
        self.current = self.current.min(tick);
    }

    /// Every peer's input for `tick`, real where known and repeat-last where not.
    ///
    /// Call this immediately before simulating `tick`; it records what was used,
    /// which is what makes a later contradiction detectable.
    pub fn inputs_for(&mut self, tick: u64) -> Vec<ResolvedInput> {
        let peers = self.peers.clone();
        let mut out = Vec::with_capacity(peers.len());
        let mut any_predicted = false;
        for peer in peers {
            let (input, real) = match self.real.get(&(peer, tick)) {
                Some(i) => (i.clone(), true),
                None => (self.repeat_last(peer, tick), false),
            };
            any_predicted |= !real;
            self.used.insert((peer, tick), input.clone());
            out.push(ResolvedInput { peer, input, real });
        }
        self.current = self.current.max(tick);
        self.simulated_ticks += 1;
        if any_predicted {
            self.predicted_ticks += 1;
        }
        out
    }

    /// The same answer as [`Rollback::inputs_for`] without recording anything —
    /// for a re-simulation, which must reuse exactly what the original tick used
    /// wherever a real input still hasn't arrived.
    pub fn replay_inputs_for(&self, tick: u64) -> Vec<ResolvedInput> {
        self.peers
            .iter()
            .map(|&peer| match self.real.get(&(peer, tick)) {
                Some(i) => ResolvedInput { peer, input: i.clone(), real: true },
                None => ResolvedInput {
                    peer,
                    // Prefer what the original pass used: re-deriving repeat-last
                    // now could pick a DIFFERENT source input (one that has since
                    // arrived for an earlier tick), making a replay disagree with
                    // the pass it is supposed to reproduce.
                    input: self
                        .used
                        .get(&(peer, tick))
                        .cloned()
                        .unwrap_or_else(|| self.repeat_last(peer, tick)),
                    real: false,
                },
            })
            .collect()
    }

    /// Record what a re-simulated tick actually used, so a *second* correction
    /// for the same tick is judged against the replay rather than the original.
    pub fn record_replay(&mut self, tick: u64, resolved: &[ResolvedInput]) {
        for r in resolved {
            self.used.insert((r.peer, tick), r.input.clone());
        }
    }

    /// Repeat-last prediction: the peer's newest real input at or before `tick`.
    /// A peer we have never heard from reads neutral, which is the only honest
    /// guess and matches an unplugged pad.
    fn repeat_last(&self, peer: PeerId, tick: u64) -> NetInput {
        let Some(&newest) = self.newest_real.get(&peer) else {
            return NetInput::default();
        };
        // Walk back from min(newest, tick): the newest real input is usually
        // exactly it, but a gap in the middle of the ring must not read as
        // neutral.
        let start = newest.min(tick.saturating_sub(1));
        for t in (0..=start).rev().take(INPUT_RING) {
            if let Some(i) = self.real.get(&(peer, t)) {
                return sustain(i);
            }
        }
        NetInput::default()
    }

    /// Newest tick where every peer's real input is known.
    fn recompute_confirmed(&mut self) {
        let mut t = self.confirmed;
        loop {
            let next = t + 1;
            if self.peers.iter().all(|p| self.real.contains_key(&(*p, next))) {
                t = next;
            } else {
                break;
            }
        }
        self.confirmed = t;
    }

    /// Drop input older than the ring. Keyed by the CONFIRMED frontier rather
    /// than by the current tick: anything at or below `confirmed` can never be
    /// re-simulated, and anything above it might.
    fn prune(&mut self) {
        let floor = self.confirmed.saturating_sub(INPUT_RING as u64);
        if floor == 0 {
            return;
        }
        self.real.retain(|(_, t), _| *t >= floor);
        self.used.retain(|(_, t), _| *t >= floor);
    }

    /// Should the driver stall rather than simulate `tick`?
    ///
    /// True when advancing would put us further ahead of the confirmed frontier
    /// than `max_depth` — at which point a correction's hitch costs more than
    /// simply waiting for the input. Stalling is what keeps a bad connection
    /// degrading into "the game runs slightly slow" rather than "the opponent
    /// teleports constantly".
    pub fn should_stall(&self, tick: u64) -> bool {
        tick > self.confirmed + self.max_depth as u64
    }

    /// 0..1 — the fraction of simulated ticks that had to guess something.
    pub fn mispredict_rate(&self) -> f32 {
        if self.simulated_ticks == 0 {
            return 0.0;
        }
        self.predicted_ticks as f32 / self.simulated_ticks as f32
    }
}

/// A repeated input with its EDGES cleared.
///
/// Repeat-last means "assume they kept holding what they held". It emphatically
/// does not mean "assume they pressed it again": replaying a `just_pressed` bit
/// on every predicted tick would fire an attack once per frame until the real
/// input arrived. Levels and axes carry over; edges are a one-tick event and
/// belong only to the tick they really happened on.
fn sustain(i: &NetInput) -> NetInput {
    NetInput {
        actions: i.actions,
        just_pressed: 0,
        just_released: 0,
        axes1: i.axes1.clone(),
        axes2: i.axes2.clone(),
        aim: i.aim,
    }
}

#[cfg(test)]
mod tests {
    /// floptle/0045: two peers whose script state differs by ONE value must be
    /// told which value, on which node, in which script.
    ///
    /// The cross-platform failure that motivated this was a single Lua number —
    /// `st.visYaw`, smoothed with `math.exp`, which is library code and is not
    /// required to agree between glibc and Windows' UCRT. One ULP a tick voided
    /// a match both players could see was identical, and the game was told
    /// nothing at all: `net.on("desync")` fired with no payload.
    #[test]
    fn a_desync_names_the_value_that_diverged() {
        use crate::session::diff_details;
        // Both peers agree about everything except one key, one ULP apart.
        let linux = vec![
            ("Player1/body".to_string(), 0x1111u64),
            ("Player2/body".to_string(), 0x2222),
            ("Player2/fighterController/hp".to_string(), 0x3333),
            ("Player2/fighterController/visYaw".to_string(), 0xa3f1),
        ];
        let mut windows = linux.clone();
        windows[3].1 = 0xa3f2;

        let out = diff_details(&[(0, linux.clone()), (1, windows.clone())]);
        assert_eq!(out.len(), 1, "exactly ONE value diverged: {out:?}");
        assert_eq!(out[0].0, "Player2/fighterController/visYaw", "named in full");
        assert_eq!(out[0].1, vec![(0, 0xa3f1), (1, 0xa3f2)], "with both peers' values");

        // Agreement anywhere else must not be reported — a list that also
        // contains everything that matched is a report that named nothing.
        assert!(diff_details(&[(0, linux.clone()), (1, linux.clone())]).is_empty());

        // One peer missing a value entirely is also a divergence: the two
        // snapshots are a different SHAPE, which is worth naming.
        let short: Vec<(String, u64)> = linux[..3].to_vec();
        let out = diff_details(&[(0, linux), (1, short)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "Player2/fighterController/visYaw");
        assert_eq!(out[0].1.len(), 1, "only the peer that HAS it reports one");

        // A single report can't disagree with anything.
        assert!(diff_details(&[(0, vec![("a".into(), 1)])]).is_empty());
    }

    use super::*;

    const P1: PeerId = 1;
    const P2: PeerId = 2;

    /// A distinguishable input per tick, so a replay that reorders or drops
    /// commands is detectable.
    fn held(tag: u64) -> NetInput {
        NetInput { actions: tag, ..Default::default() }
    }

    fn press(tag: u64) -> NetInput {
        NetInput { actions: tag, just_pressed: tag, ..Default::default() }
    }

    fn rb(delay: u8) -> Rollback {
        Rollback::new(P1, vec![P1, P2], delay)
    }

    /// FIELD REGRESSION (floptle/0049): the input delay has to be choosable,
    /// because the constant 2 is right only for peers in the same building.
    ///
    /// Both sides of this run the SAME inputs over the same link — one-way
    /// latency of four ticks, which is the 112 ms RTT two players in different
    /// houses actually measured. The only difference is the delay. At 2 the
    /// opponent's input lands two ticks after the tick that needed it, on
    /// every tick, forever; at 5 it lands before.
    ///
    /// Nothing is broken at delay 2 — every peer agrees, the checksum is green
    /// — it just does several times the work and feels like it. That is the
    /// whole argument for putting the number next to the measurement.
    #[test]
    fn a_delay_that_covers_the_link_stops_guessing_every_tick() {
        /// One-way trip, in ticks: 56 ms at 60 Hz.
        const LAG: u64 = 4;
        const TICKS: u64 = 300;

        fn run(delay: u8) -> f32 {
            let mut r = rb(delay);
            for t in 1..=TICKS {
                // We sample for tick `t` and it applies at `t + delay`.
                r.add_local(t, held(t));
                // The opponent sampled the same tick and it applied at
                // `t + delay` for them too — but it reaches us LAG ticks of
                // wall clock later, i.e. we only see it once we are simulating
                // tick `t + LAG`.
                if t > LAG {
                    let theirs = t - LAG;
                    r.add_remote(P2, theirs + u64::from(delay), held(theirs));
                }
                // …and then we simulate the tick that is due now.
                r.inputs_for(t);
            }
            r.mispredict_rate()
        }

        let tight = run(2);
        let roomy = run(5);
        // The constant guesses essentially every tick…
        assert!(tight > 0.9, "delay 2 over a 4-tick link should guess nearly always, got {tight}");
        // …and one that covers the link essentially never does, once the
        // opening ticks (which have nothing to go on either way) are behind it.
        assert!(roomy < 0.09, "delay 5 should cover the same link, got {roomy}");
        assert!(
            tight > roomy * 10.0,
            "an order of magnitude apart on the same inputs: {tight} vs {roomy}"
        );
    }

    #[test]
    fn local_input_is_applied_after_the_delay() {
        let mut r = rb(3);
        assert_eq!(r.add_local(10, held(1)), 13, "sampled at 10, applied at 13");
        let got = r.inputs_for(13);
        assert_eq!(got.iter().find(|g| g.peer == P1).unwrap().input, held(1));
        assert!(got.iter().find(|g| g.peer == P1).unwrap().real);
        // Delay is clamped, so a silly config can't push input a second into the
        // future.
        assert_eq!(Rollback::new(P1, vec![P1], 200).delay(), MAX_DELAY);
    }

    #[test]
    fn a_missing_remote_input_repeats_their_last_one() {
        let mut r = rb(0);
        r.add_remote(P2, 5, held(0b110));
        // Tick 6 has nothing from P2 → repeat tick 5's.
        let got = r.inputs_for(6);
        let p2 = got.iter().find(|g| g.peer == P2).unwrap();
        assert_eq!(p2.input.actions, 0b110);
        assert!(!p2.real, "and it must be marked as a guess");
    }

    /// The trap that makes a repeated input worse than useless: an edge is a
    /// one-tick event. Repeating `just_pressed` would fire the attack again on
    /// every predicted tick.
    #[test]
    fn a_repeated_input_never_repeats_its_edges() {
        let mut r = rb(0);
        r.add_remote(P2, 1, press(0b1));
        let got = r.inputs_for(2);
        let p2 = got.iter().find(|g| g.peer == P2).unwrap();
        assert_eq!(p2.input.actions, 0b1, "still held");
        assert_eq!(p2.input.just_pressed, 0, "but NOT pressed again");
        assert_eq!(p2.input.just_released, 0);
    }

    #[test]
    fn a_peer_never_heard_from_reads_neutral() {
        let mut r = rb(0);
        let got = r.inputs_for(1);
        let p2 = got.iter().find(|g| g.peer == P2).unwrap();
        assert_eq!(p2.input, NetInput::default());
        assert!(!p2.real);
    }

    #[test]
    fn an_arrival_matching_the_prediction_costs_nothing() {
        let mut r = rb(0);
        r.add_remote(P2, 1, held(0b10));
        r.inputs_for(1);
        r.inputs_for(2); // predicts 0b10 for P2
        // …and that's exactly what they sent. No correction: this is the common
        // case (a player holding a direction) and it must stay free.
        assert_eq!(r.add_remote(P2, 2, held(0b10)), None);
        assert_eq!(r.corrections, 0);
    }

    #[test]
    fn a_contradicting_arrival_names_the_tick_and_depth() {
        let mut r = rb(0);
        r.add_remote(P2, 1, held(0b10));
        for t in 1..=5 {
            r.inputs_for(t);
        }
        // P2 actually let go on tick 3; we predicted they were still holding.
        let c = r.add_remote(P2, 3, held(0)).expect("must correct");
        assert_eq!(c.tick, 3);
        assert_eq!(c.depth, 3, "ticks 3, 4 and 5 were simulated wrong");
        assert_eq!(r.last_depth, 3);
        assert_eq!(r.max_depth_seen, 3);
    }

    #[test]
    fn a_duplicate_arrival_is_free() {
        let mut r = rb(0);
        r.add_remote(P2, 1, held(0b10));
        r.inputs_for(1);
        // The redundant window resends the last few ticks in every packet.
        assert_eq!(r.add_remote(P2, 1, held(0b10)), None);
        assert_eq!(r.corrections, 0);
    }

    #[test]
    fn an_input_for_the_future_never_corrects() {
        let mut r = rb(0);
        r.inputs_for(1);
        // Arriving early is the goal, not a problem.
        assert_eq!(r.add_remote(P2, 9, held(0b1)), None);
        assert_eq!(r.corrections, 0);
    }

    #[test]
    fn the_confirmed_frontier_advances_only_when_every_peer_is_heard() {
        let mut r = rb(0);
        r.add_local(1, held(1));
        assert_eq!(r.confirmed(), 0, "P2 hasn't been heard for tick 1");
        r.add_remote(P2, 1, held(1));
        assert_eq!(r.confirmed(), 1);
        // A gap holds the frontier even though tick 3 is fully known.
        r.add_local(2, held(1));
        r.add_local(3, held(1));
        r.add_remote(P2, 3, held(1));
        assert_eq!(r.confirmed(), 1, "tick 2 is missing from P2");
        r.add_remote(P2, 2, held(1));
        assert_eq!(r.confirmed(), 3, "and now it jumps past the filled gap");
    }

    #[test]
    fn a_departed_peer_stops_holding_the_frontier_back() {
        let mut r = rb(0);
        r.add_local(1, held(1));
        assert_eq!(r.confirmed(), 0);
        r.set_peers(vec![P1]); // P2 disconnected
        assert_eq!(r.confirmed(), 1, "nobody is left to wait for");
    }

    /// Without the warm-up seed the confirmed frontier can never leave 0 —
    /// ticks 1..=delay hold it back forever — and the session stalls against
    /// the depth cap a few ticks into the match with nothing to wait for.
    #[test]
    fn the_delay_warmup_is_seeded_so_the_frontier_can_advance() {
        let mut r = rb(2);
        let seeded = r.prime_warmup();
        assert_eq!(seeded.len(), 2, "delay 2 leaves ticks 1 and 2 unsampled");
        assert_eq!(seeded[0], (1, NetInput::default()));
        // The peer seeds and ships its own; ours arrive over the wire.
        for (applied, input) in [(1, NetInput::default()), (2, NetInput::default())] {
            r.add_remote(P2, applied, input);
        }
        assert_eq!(r.confirmed(), 2, "the frontier clears the warm-up");
        // …and a real sample still lands where the delay puts it.
        assert_eq!(r.add_local(1, held(9)), 3);
        r.add_remote(P2, 3, held(9));
        assert_eq!(r.confirmed(), 3);
        // Delay 0 has no warm-up to seed.
        assert!(rb(0).prime_warmup().is_empty());
    }

    #[test]
    fn stalling_kicks_in_past_the_depth_cap() {
        let mut r = rb(0);
        r.max_depth = 4;
        r.add_local(1, held(1));
        r.add_remote(P2, 1, held(1)); // confirmed = 1
        assert!(!r.should_stall(5), "1 + 4 is still allowed");
        assert!(r.should_stall(6), "further than the cap: wait instead");
    }

    /// A replay must reuse exactly what the original pass used wherever a real
    /// input still hasn't arrived. Re-deriving repeat-last during the replay
    /// could pick a different source and make the replay disagree with the pass
    /// it is meant to reproduce — a desync that only shows up under packet loss.
    #[test]
    fn a_replay_reuses_the_original_guess_for_still_missing_inputs() {
        let mut r = rb(0);
        r.add_remote(P2, 1, held(0b100));
        r.inputs_for(1);
        r.inputs_for(2); // guessed 0b100 for tick 2
        r.inputs_for(3); // and for tick 3
        // Tick 2's real input arrives and contradicts.
        let c = r.add_remote(P2, 2, held(0b1)).expect("correct");
        assert_eq!(c.tick, 2);
        // Replaying tick 2 uses the real input; tick 3 still has none, and must
        // reuse the ORIGINAL guess rather than re-deriving from the new tick 2.
        let t2 = r.replay_inputs_for(2);
        assert_eq!(t2.iter().find(|g| g.peer == P2).unwrap().input.actions, 0b1);
        let t3 = r.replay_inputs_for(3);
        let p2 = t3.iter().find(|g| g.peer == P2).unwrap();
        assert_eq!(p2.input.actions, 0b100, "the guess tick 3 actually ran with");
        assert!(!p2.real);
    }

    /// After a replay records what it used, a SECOND correction for the same
    /// tick is judged against the replay — not against the original pass, which
    /// no longer describes any state that exists.
    #[test]
    fn a_second_correction_is_judged_against_the_replay() {
        let mut r = rb(0);
        r.add_remote(P2, 1, held(0b100));
        for t in 1..=3 {
            r.inputs_for(t);
        }
        r.add_remote(P2, 2, held(0b1)).expect("first correction");
        // Replay 2..=3 with the corrected inputs and record what ran.
        for t in 2..=3 {
            let resolved = r.replay_inputs_for(t);
            r.record_replay(t, &resolved);
        }
        // Tick 3's real input now arrives. The replay guessed 0b100 for it
        // (carried from the original pass), so 0b100 must NOT correct again…
        assert_eq!(r.add_remote(P2, 3, held(0b100)), None, "matches what the replay ran");
        // …while something else must.
        let mut r2 = rb(0);
        r2.add_remote(P2, 1, held(0b100));
        for t in 1..=3 {
            r2.inputs_for(t);
        }
        r2.add_remote(P2, 2, held(0b1)).unwrap();
        for t in 2..=3 {
            let resolved = r2.replay_inputs_for(t);
            r2.record_replay(t, &resolved);
        }
        assert!(r2.add_remote(P2, 3, held(0b1000)).is_some(), "a real disagreement corrects");
    }

    #[test]
    fn the_mispredict_rate_counts_ticks_that_had_to_guess() {
        let mut r = rb(0);
        for t in 1..=4 {
            r.add_local(t, held(1));
            r.add_remote(P2, t, held(1));
        }
        for t in 1..=4 {
            r.inputs_for(t);
        }
        assert_eq!(r.mispredict_rate(), 0.0, "everything was known in advance");
        r.add_local(5, held(1));
        r.inputs_for(5); // P2 unknown → guessed
        assert!((r.mispredict_rate() - 0.2).abs() < 1e-6, "1 of 5");
    }

    #[test]
    fn old_input_is_pruned_but_the_unconfirmed_window_is_kept() {
        let mut r = rb(0);
        for t in 1..=(INPUT_RING as u64 + 50) {
            r.add_local(t, held(t));
            r.add_remote(P2, t, held(t));
        }
        assert_eq!(r.confirmed(), INPUT_RING as u64 + 50);
        // Everything within the ring of the frontier survives…
        assert!(r.real.contains_key(&(P2, INPUT_RING as u64 + 50)));
        assert!(r.real.contains_key(&(P2, 51)));
        // …and what fell off is genuinely gone, not leaking forever.
        assert!(!r.real.contains_key(&(P2, 1)));
    }
}
