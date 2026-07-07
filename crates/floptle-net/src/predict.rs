//! Client-side prediction bookkeeping (`docs/netcode-design.md` §6): the
//! input/predicted-state ring, reconciliation against authoritative
//! snapshots, and the visual error-smoothing offset.
//!
//! The [`Predictor`] is pure bookkeeping — it decides *whether* and *what* to
//! replay; the driver (editor play loop / headless runtime) owns the actual
//! re-simulation, because that means running the node's `fixedUpdate` + its
//! physics body, which live outside this crate. The loop is:
//!
//! 1. Each tick: simulate locally, then [`Predictor::record`] (tick, input,
//!    post-tick state).
//! 2. On an authoritative state for tick `t`: [`Predictor::reconcile`]. Within
//!    epsilon → done (prediction confirmed). Diverged → the driver restores the
//!    body to the server state and replays the returned `(tick, input)` list
//!    through `fixedUpdate` + [`step_body_tick`], calling
//!    [`Predictor::rerecord`] for each replayed tick.
//! 3. The visual error (how far the rendered position jumped) accumulates in
//!    [`Predictor::error_offset`] and decays via [`Predictor::decay_error`] —
//!    the renderer adds the offset so corrections read as a nudge, not a snap.

use std::collections::VecDeque;

use crate::wire::NetInput;

/// A predicted node's full dynamic state at the end of a tick, in absolute
/// world coordinates. Mirrors the physics `BodySnapshot` + rotation without a
/// physics dependency; the driver converts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PredictedState {
    pub pos: [f64; 3],
    pub rot: [f32; 4],
    pub vel: [f32; 3],
    pub grounded: bool,
}

/// How many ticks of history the ring keeps (~2 s at 60 Hz — far beyond any
/// playable RTT; older entries are confirmed-or-lost either way).
const RING_CAP: usize = 128;

/// The default reconcile epsilon, metres: server/client positions closer than
/// this count as "prediction confirmed" (f32 physics wobble, not divergence).
pub const DEFAULT_EPSILON: f64 = 1e-3;

pub struct Predictor {
    /// (tick, the input that produced it, the state at END of that tick).
    ring: VecDeque<(u64, NetInput, PredictedState)>,
    /// Rendered-position error introduced by the last correction (world
    /// metres), decayed toward zero each tick and ADDED to the rendered
    /// transform so the correction is smoothed over ~100 ms.
    pub error_offset: [f64; 3],
    /// Per-tick decay factor for `error_offset` (0.7 ≈ gone in ~6 ticks).
    pub error_decay: f64,
}

impl Default for Predictor {
    fn default() -> Self {
        Self::new()
    }
}

impl Predictor {
    pub fn new() -> Self {
        Self { ring: VecDeque::new(), error_offset: [0.0; 3], error_decay: 0.7 }
    }

    /// Record a locally-simulated tick.
    pub fn record(&mut self, tick: u64, input: NetInput, state: PredictedState) {
        self.ring.push_back((tick, input, state));
        while self.ring.len() > RING_CAP {
            self.ring.pop_front();
        }
    }

    /// Overwrite the stored state for a REPLAYED tick (the input is unchanged —
    /// replay re-derives states from the same inputs off the corrected base).
    pub fn rerecord(&mut self, tick: u64, state: PredictedState) {
        if let Some(entry) = self.ring.iter_mut().find(|(t, _, _)| *t == tick) {
            entry.2 = state;
        }
    }

    /// The stored prediction for a tick, if still in the ring.
    pub fn predicted_at(&self, tick: u64) -> Option<&PredictedState> {
        self.ring.iter().find(|(t, _, _)| *t == tick).map(|(_, _, s)| s)
    }

    /// An authoritative state arrived for `tick`. Returns `None` when the
    /// prediction matched (within `eps` metres — confirmed, nothing to do), or
    /// the `(tick, input)` list to replay IN ORDER after the driver restores
    /// the body to `server`'s state. Also drops ring entries `<= tick`
    /// (confirmed history) and accrues the visual error offset from the
    /// correction (old predicted position − server position at `tick`).
    pub fn reconcile(
        &mut self,
        tick: u64,
        server: &PredictedState,
        eps: f64,
    ) -> Option<Vec<(u64, NetInput)>> {
        let predicted = self.predicted_at(tick).copied();
        // Trim confirmed history either way.
        while self.ring.front().is_some_and(|(t, _, _)| *t <= tick) {
            self.ring.pop_front();
        }
        let Some(p) = predicted else {
            // Nothing recorded for that tick (too old / just spawned): adopt
            // the server state and replay everything we still have.
            return Some(self.ring.iter().map(|(t, i, _)| (*t, i.clone())).collect());
        };
        let d = [p.pos[0] - server.pos[0], p.pos[1] - server.pos[1], p.pos[2] - server.pos[2]];
        let dist2 = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
        if dist2 <= eps * eps {
            return None; // confirmed
        }
        // The renderer was showing the (wrong) predicted trajectory; keep the
        // visual difference as an offset to decay, so the correction is smooth.
        self.error_offset[0] += d[0];
        self.error_offset[1] += d[1];
        self.error_offset[2] += d[2];
        Some(self.ring.iter().map(|(t, i, _)| (*t, i.clone())).collect())
    }

    /// Decay the visual error offset one tick (call every gameplay tick).
    pub fn decay_error(&mut self) {
        for c in &mut self.error_offset {
            *c *= self.error_decay;
            if c.abs() < 1e-5 {
                *c = 0.0;
            }
        }
    }

    /// Ticks currently held (diagnostics / the net-stats overlay).
    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(x: f64) -> PredictedState {
        PredictedState { pos: [x, 0.0, 0.0], rot: [0.0, 0.0, 0.0, 1.0], vel: [1.0, 0.0, 0.0], grounded: true }
    }

    fn input(tag: &str) -> NetInput {
        NetInput { keys_down: vec![tag.into()], ..Default::default() }
    }

    #[test]
    fn confirmed_prediction_replays_nothing() {
        let mut p = Predictor::new();
        for t in 1..=10 {
            p.record(t, input(&format!("k{t}")), state(t as f64));
        }
        // Server agrees with tick 5 (within eps): no replay, history trimmed.
        assert_eq!(p.reconcile(5, &state(5.0 + 1e-6), DEFAULT_EPSILON), None);
        assert_eq!(p.len(), 5, "ticks <= 5 are confirmed and dropped");
        assert_eq!(p.error_offset, [0.0; 3]);
    }

    #[test]
    fn divergence_replays_unacked_inputs_in_order() {
        let mut p = Predictor::new();
        for t in 1..=10 {
            p.record(t, input(&format!("k{t}")), state(t as f64));
        }
        // Server says tick 6 was actually at x=3 (we predicted 6): replay 7..10.
        let replay = p.reconcile(6, &state(3.0), DEFAULT_EPSILON).expect("must replay");
        assert_eq!(replay.len(), 4);
        assert_eq!(replay[0].0, 7);
        assert_eq!(replay[3].0, 10);
        assert_eq!(replay[1].1.keys_down, vec!["k8".to_string()], "inputs preserved");
        // Visual error captured: predicted 6.0 vs server 3.0.
        assert!((p.error_offset[0] - 3.0).abs() < 1e-9);
        // Replay rerecords corrected states.
        p.rerecord(7, state(4.0));
        assert_eq!(p.predicted_at(7).unwrap().pos[0], 4.0);
    }

    #[test]
    fn error_offset_decays_to_zero() {
        let mut p = Predictor::new();
        p.error_offset = [3.0, 0.0, -1.5];
        for _ in 0..40 {
            p.decay_error();
        }
        assert_eq!(p.error_offset, [0.0; 3], "offset must fully decay (snap-to-zero floor)");
    }

    #[test]
    fn unknown_tick_adopts_server_and_replays_everything_left() {
        let mut p = Predictor::new();
        for t in 100..=103 {
            p.record(t, input("w"), state(t as f64));
        }
        // Server reports tick 50 — older than anything we kept.
        let replay = p.reconcile(50, &state(0.0), DEFAULT_EPSILON).expect("adopt + replay");
        assert_eq!(replay.len(), 4, "all held ticks replay off the adopted base");
    }
}
