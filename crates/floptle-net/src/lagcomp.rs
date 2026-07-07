//! Lag-compensation history (`docs/netcode-design.md` §7): the server keeps a
//! short ring of authoritative per-tick state — networked entities' positions
//! plus their `synced` combat vars — so a combat intent stamped with the tick
//! its sender PERCEIVED can be judged against the world as that player saw it.
//!
//! The [`LagHistory`] is pure bookkeeping, like [`crate::Predictor`]: the
//! driver records after each authoritative tick and looks poses back up when
//! an RPC carries a perceived tick; actually re-posing colliders around the
//! handler is the script layer's job (`net.rewind`).

use std::collections::VecDeque;

use crate::value::NetValue;

/// How far back a rewind may reach, ticks (250 ms at 60 Hz). The clamp is the
/// fairness/safety tradeoff of the genre: beyond it a high-ping attacker would
/// be shooting everyone else too far in their past.
pub const MAX_REWIND_TICKS: u64 = 15;

/// Ticks of history kept (~666 ms at 60 Hz — the clamp plus slack, so a
/// maximally-late intent still finds its tick recorded).
const HISTORY_CAP: usize = 40;

/// One entity's recorded state at a tick: world position plus its scripts'
/// `synced` vars (script kind → name/value pairs) — a parry flag must be read
/// from the SAME rewound tick as the pose it's judged against.
#[derive(Clone, Debug, Default)]
pub struct HistEntry {
    pub pos: [f64; 3],
    pub synced: Vec<(String, Vec<(String, NetValue)>)>,
}

/// The server's per-tick state ring, keyed by NetId.
#[derive(Default)]
pub struct LagHistory {
    ring: VecDeque<(u64, Vec<(u64, HistEntry)>)>,
}

impl LagHistory {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one authoritative tick's state (call after physics writeback,
    /// with post-tick transforms). Ticks must be recorded in order.
    pub fn record(&mut self, tick: u64, states: Vec<(u64, HistEntry)>) {
        self.ring.push_back((tick, states));
        while self.ring.len() > HISTORY_CAP {
            self.ring.pop_front();
        }
    }

    /// The recorded state of entity `id` at `tick` — exact when recorded, else
    /// the newest recorded tick BEFORE it (the state that was still current),
    /// else the oldest held (deeper-than-history rewinds get the edge, which
    /// the [`MAX_REWIND_TICKS`] clamp keeps honest). None if the entity has no
    /// recorded state at all.
    pub fn state_at(&self, id: u64, tick: u64) -> Option<&HistEntry> {
        let mut best: Option<&HistEntry> = None;
        for (t, states) in &self.ring {
            let here = states.iter().find(|(i, _)| *i == id).map(|(_, s)| s);
            if *t <= tick {
                best = here.or(best);
            } else {
                return best.or(here); // past the target: oldest-held fallback
            }
        }
        best
    }

    /// Clamp a perceived tick into the legal rewind window ending at `now`:
    /// no further back than [`MAX_REWIND_TICKS`], never into the future.
    pub fn clamp_rewind(now: u64, perceived: u64) -> u64 {
        perceived.clamp(now.saturating_sub(MAX_REWIND_TICKS), now)
    }

    /// Ticks currently held (diagnostics).
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

    fn entry(x: f64) -> HistEntry {
        HistEntry {
            pos: [x, 0.0, 0.0],
            synced: vec![("combat".into(), vec![("parrying".into(), NetValue::Bool(x > 5.0))])],
        }
    }

    #[test]
    fn exact_and_nearest_before_lookup() {
        let mut h = LagHistory::new();
        // Recorded every other tick (like a 30 Hz snapshot world would look).
        for t in [10u64, 12, 14] {
            h.record(t, vec![(1, entry(t as f64))]);
        }
        assert_eq!(h.state_at(1, 12).unwrap().pos[0], 12.0, "exact tick");
        assert_eq!(h.state_at(1, 13).unwrap().pos[0], 12.0, "nearest recorded before");
        assert_eq!(h.state_at(1, 99).unwrap().pos[0], 14.0, "future clamps to newest");
        assert_eq!(h.state_at(1, 3).unwrap().pos[0], 10.0, "too old gets the oldest held");
        assert!(h.state_at(42, 12).is_none(), "unknown entity");
    }

    #[test]
    fn synced_vars_ride_the_same_tick() {
        let mut h = LagHistory::new();
        h.record(10, vec![(1, entry(2.0))]); // parrying = false
        h.record(11, vec![(1, entry(9.0))]); // parrying = true
        let at10 = &h.state_at(1, 10).unwrap().synced[0].1[0];
        let at11 = &h.state_at(1, 11).unwrap().synced[0].1[0];
        assert_eq!(at10.1, NetValue::Bool(false));
        assert_eq!(at11.1, NetValue::Bool(true));
    }

    #[test]
    fn ring_is_bounded() {
        let mut h = LagHistory::new();
        for t in 0..200u64 {
            h.record(t, vec![(1, entry(t as f64))]);
        }
        assert_eq!(h.len(), 40);
        assert_eq!(h.state_at(1, 0).unwrap().pos[0], 160.0, "oldest held is the floor");
    }

    #[test]
    fn rewind_clamp() {
        assert_eq!(LagHistory::clamp_rewind(100, 95), 95, "inside the window");
        assert_eq!(LagHistory::clamp_rewind(100, 50), 85, "too deep clamps to 250 ms");
        assert_eq!(LagHistory::clamp_rewind(100, 120), 100, "never the future");
        assert_eq!(LagHistory::clamp_rewind(5, 0), 0, "early ticks saturate");
    }
}
