//! Interest management (`docs/netcode-design.md` §5.2) — the player-count
//! feature.
//!
//! Replicating everything to everyone is right up to a few dozen players and
//! then melts: cost per client grows with the *world's* population, so the
//! ceiling is set by the busiest moment rather than by what any one player can
//! see. This makes each client pay for its neighbourhood instead. A far-away
//! idle crate syncs eventually; a nearby fighting player syncs every snapshot.
//!
//! Two mechanisms, and they do different jobs:
//!
//! - **Relevance** decides what a client is *allowed* to hear about — a radius
//!   around its own avatar, plus whatever is flagged always-relevant, plus
//!   anything it owns. Hysteresis keeps a node hovering on the boundary from
//!   flickering in and out.
//! - **The priority accumulator** decides what it hears about *this snapshot*,
//!   because relevance alone still overflows a link when a hundred relevant
//!   things all move. Every relevant node accrues priority each snapshot; the
//!   budget is spent newest-and-nearest-first; whatever misses out keeps its
//!   priority and wins the next round. **Nothing is dropped, only deferred** —
//!   which is what makes this safe to turn on without auditing a game for
//!   things that must never be missed.
//!
//! ## What leaving the set means
//!
//! The design says an entity leaving a client's set despawns there and respawns
//! on re-entry. That is right for **runtime spawns** — the server holds their
//! authoring data (`spawned_docs`) and can recreate them exactly. It is wrong
//! for **scene-authored** nodes: the client already has them from the scene
//! file, the server has no ron to send back, and despawning one would remove it
//! for good. So a scene node that goes irrelevant simply stops being updated
//! (it is out of sight by construction) and is sent in full the moment it
//! becomes relevant again. Same bandwidth win, no unrecoverable state.

use std::collections::{HashMap, HashSet};

use crate::transport::PeerId;

/// Server-side interest configuration. Off by default: broadcasting is cheaper
/// and simpler below a few dozen players, and a feature that changes what
/// arrives on the wire should be something a project turns on deliberately.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InterestConfig {
    pub enabled: bool,
    /// Metres around a client's own avatar.
    pub radius: f64,
    /// Extra metres a node may drift beyond `radius` before it stops being
    /// relevant, once it already is. Pure anti-flicker: without it, a node
    /// sitting exactly on the boundary enters and leaves every snapshot.
    pub hysteresis: f64,
    /// Per-client entity-entry budget, bytes per second. Spent on snapshot
    /// entries only — control traffic, RPCs and inputs are never rationed.
    pub budget_bytes_per_sec: u32,
}

impl Default for InterestConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            radius: 150.0,
            hysteresis: 25.0,
            budget_bytes_per_sec: 16 * 1024,
        }
    }
}

impl InterestConfig {
    /// The budget for ONE snapshot, given how many go out per second.
    pub fn budget_per_snapshot(&self, snapshots_per_sec: f32) -> usize {
        if snapshots_per_sec <= 0.0 {
            return usize::MAX;
        }
        (self.budget_bytes_per_sec as f32 / snapshots_per_sec).max(64.0) as usize
    }
}

/// What one client's last snapshot actually cost, for the 🌐 panel.
///
/// Interest management is the one netcode feature whose whole job is to *not*
/// send things, which makes it invisible from the outside: a project that
/// turns it on and sets the radius too tight gets a world where distant
/// objects quietly stop moving, and nothing anywhere says why. These counters
/// are what let a developer see the trade they are making — how much of the
/// world each client is being told about, and whether the budget is the thing
/// holding the rest back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InterestStat {
    /// Entities this client is allowed to hear about at all.
    pub relevant: usize,
    /// Entities whose state actually went out in the last snapshot.
    pub sent: usize,
    /// Wanted a turn and didn't get one — the budget was spent. Non-zero is
    /// not a fault (they accrue priority and go next), but a number that never
    /// comes back down means the budget is too small for the scene.
    pub deferred: usize,
    /// Bytes of entity entries in the last snapshot.
    pub bytes: usize,
}

/// One candidate entity, as the accumulator sees it.
#[derive(Clone, Copy, Debug)]
pub struct Candidate {
    pub id: u64,
    /// Distance from the client's avatar, metres. `None` when the client has
    /// no avatar to measure from (a spectator), which makes everything
    /// equally near rather than equally far.
    pub distance: Option<f64>,
    /// Its transform changed since this client last heard about it.
    pub changed: bool,
    /// It belongs to some player (an avatar rather than scenery).
    pub is_player: bool,
    /// It belongs to THIS client.
    pub is_owned: bool,
    /// Flagged never-cull.
    pub always: bool,
    /// Encoded size of the entry it would produce, bytes.
    pub cost: usize,
}

/// What one client is currently being told about, and how badly each thing it
/// is not being told about wants a turn.
#[derive(Clone, Debug, Default)]
pub struct PeerInterest {
    /// Accrued priority per net id — reset to zero when the entity is sent.
    priority: HashMap<u64, f32>,
    /// Ids this client is considered to hold current state for. A node
    /// entering this set is sent in FULL, whatever the delta says.
    live: HashSet<u64>,
}

impl PeerInterest {
    pub fn is_live(&self, id: u64) -> bool {
        self.live.contains(&id)
    }

    pub fn forget(&mut self, id: u64) {
        self.priority.remove(&id);
        self.live.remove(&id);
    }

    /// Everything this client holds that is no longer relevant.
    pub fn stale(&self, relevant: &HashSet<u64>) -> Vec<u64> {
        self.live.iter().copied().filter(|id| !relevant.contains(id)).collect()
    }

    /// Accrue priority for every candidate, then spend `budget` on the
    /// hungriest — returning the ids to send, in the order they were chosen.
    ///
    /// Priority is *accrued*, not computed fresh, which is the whole trick: a
    /// node that keeps losing to closer, busier neighbours climbs until it
    /// wins. Starvation is impossible as long as it stays relevant.
    pub fn choose(
        &mut self,
        candidates: &[Candidate],
        radius: f64,
        budget: usize,
    ) -> Vec<u64> {
        for c in candidates {
            let mut gain = 1.0_f32;
            if c.changed {
                gain += 2.0;
            }
            if c.is_player {
                gain += 1.5;
            }
            if c.is_owned {
                // Your own avatar is the one thing that must never wait. It is
                // also what the client reconciles its prediction against.
                gain += 100.0;
            }
            if c.always {
                gain += 50.0;
            }
            // Nearer is more urgent, and a node the client has never seen
            // outranks one it merely has slightly stale news about.
            if let Some(d) = c.distance
                && radius > 0.0
            {
                gain += 2.0 * (1.0 - (d / radius).clamp(0.0, 1.0)) as f32;
            }
            if !self.live.contains(&c.id) {
                gain += 4.0;
            }
            *self.priority.entry(c.id).or_insert(0.0) += gain;
        }
        let mut ranked: Vec<(u64, f32, usize)> = candidates
            .iter()
            .map(|c| (c.id, self.priority.get(&c.id).copied().unwrap_or(0.0), c.cost))
            .collect();
        // Ties broken by id so two servers with the same state make the same
        // choice — a test that sometimes sends a different set is not a test.
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));

        let mut spent = 0usize;
        let mut out = Vec::new();
        for (id, _, cost) in ranked {
            if spent + cost > budget && !out.is_empty() {
                continue; // over budget: keep the accrued priority for next time
            }
            spent += cost;
            out.push(id);
            self.priority.insert(id, 0.0);
            self.live.insert(id);
        }
        out
    }
}

/// Every client's interest state, keyed by peer.
#[derive(Debug, Default)]
pub struct InterestSets(HashMap<PeerId, PeerInterest>);

impl InterestSets {
    pub fn get_mut(&mut self, peer: PeerId) -> &mut PeerInterest {
        self.0.entry(peer).or_default()
    }

    pub fn drop_peer(&mut self, peer: PeerId) {
        self.0.remove(&peer);
    }

    /// Forget an entity everywhere — it despawned for real.
    pub fn forget_everywhere(&mut self, id: u64) {
        for pi in self.0.values_mut() {
            pi.forget(id);
        }
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(id: u64, distance: f64, changed: bool) -> Candidate {
        Candidate {
            id,
            distance: Some(distance),
            changed,
            is_player: false,
            is_owned: false,
            always: false,
            cost: 10,
        }
    }

    #[test]
    fn a_budget_that_fits_everything_sends_everything() {
        let mut pi = PeerInterest::default();
        let cs: Vec<Candidate> = (1..=5).map(|i| cand(i, 10.0, true)).collect();
        let sent = pi.choose(&cs, 150.0, 1000);
        assert_eq!(sent.len(), 5, "nothing should be held back when there is room");
    }

    #[test]
    fn over_budget_the_nearest_and_busiest_go_first() {
        let mut pi = PeerInterest::default();
        let cs = vec![cand(1, 140.0, false), cand(2, 5.0, true)];
        let sent = pi.choose(&cs, 150.0, 10); // room for exactly one
        assert_eq!(sent, vec![2], "the near, moving one wins the single slot");
    }

    /// The property the whole accumulator exists for: losing a round must not
    /// mean losing forever. A far-away node that keeps being outranked climbs
    /// until it gets its turn.
    #[test]
    fn nothing_starves_however_long_it_loses() {
        let mut pi = PeerInterest::default();
        let near = cand(1, 1.0, true);
        let far = cand(2, 149.0, false);
        let mut far_sent = 0;
        for _ in 0..40 {
            let sent = pi.choose(&[near, far], 150.0, 10); // one slot per round
            if sent.contains(&2) {
                far_sent += 1;
            }
        }
        assert!(far_sent > 0, "a node that never wins is a node that is silently broken");
    }

    /// Your own avatar is what your prediction reconciles against. It cannot
    /// ever be the thing that gets deferred.
    #[test]
    fn your_own_avatar_outranks_everything() {
        let mut pi = PeerInterest::default();
        let mut mine = cand(9, 0.0, false);
        mine.is_owned = true;
        mine.is_player = true;
        let crowd: Vec<Candidate> = (1..=8).map(|i| cand(i, 2.0, true)).collect();
        let mut all = crowd.clone();
        all.push(mine);
        let sent = pi.choose(&all, 150.0, 10);
        assert_eq!(sent, vec![9], "the owner's own node takes the only slot");
    }

    #[test]
    fn a_never_seen_entity_is_sent_in_full_and_then_tracked() {
        let mut pi = PeerInterest::default();
        assert!(!pi.is_live(1), "unknown until it has actually been sent");
        pi.choose(&[cand(1, 1.0, true)], 150.0, 1000);
        assert!(pi.is_live(1), "sending it is what makes the client's copy current");
    }

    #[test]
    fn what_falls_out_of_the_relevant_set_is_reported_as_stale() {
        let mut pi = PeerInterest::default();
        pi.choose(&[cand(1, 1.0, true), cand(2, 2.0, true)], 150.0, 1000);
        let relevant: HashSet<u64> = [1].into_iter().collect();
        assert_eq!(pi.stale(&relevant), vec![2]);
    }

    #[test]
    fn the_budget_divides_by_the_snapshot_rate() {
        let c = InterestConfig::default();
        // 16 KB/s at 30 snapshots/s is ~546 bytes of entries per snapshot.
        assert_eq!(c.budget_per_snapshot(30.0), 546);
        // A pathological rate must not produce a zero budget, which would
        // silently send nothing forever.
        assert!(c.budget_per_snapshot(100_000.0) >= 64);
    }

    #[test]
    fn one_entry_always_fits_however_small_the_budget() {
        // Otherwise a budget smaller than one entry stalls the session
        // completely, and the symptom (nothing replicates) looks nothing like
        // the cause (a mistyped budget).
        let mut pi = PeerInterest::default();
        let mut big = cand(1, 1.0, true);
        big.cost = 10_000;
        assert_eq!(pi.choose(&[big], 150.0, 64), vec![1]);
    }
}
