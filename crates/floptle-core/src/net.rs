//! Networking identity + replication description (Phase 2a of ADR-0022 —
//! see `docs/netcode-design.md`).
//!
//! These are the *authored/data* halves of the netcode: which nodes replicate
//! and how, plus the stable network identity. The behavior (sessions, wire
//! format, prediction) lives in `floptle-net`; keeping the components here
//! mirrors how `RigidBody` (data, core) pairs with `floptle-physics` (behavior).

/// Stable network identity for a replicated object — the id both sides of the
/// wire agree on. Distinct from [`crate::Entity`], whose generational index is
/// per-session and NOT stable across scene reloads or machines.
///
/// Scene-authored networked nodes get a deterministic id derived from the scene
/// (so a level's static networked set needs no spawn messages); runtime spawns
/// allocate from a server counter. `0` = unassigned (not yet in a session).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NetId(pub u64);

impl NetId {
    /// Not yet assigned by a session.
    pub const UNASSIGNED: NetId = NetId(0);

    pub fn is_assigned(self) -> bool {
        self.0 != 0
    }
}

/// How clients treat a replicated node (see `docs/netcode-design.md` §4.2).
///
/// Authority is binary and server-rooted: the server owns everything.
/// `Predicted` is not client authority — it's client *optimism*: the owning
/// client also simulates locally, ahead of the server, and the server's word
/// remains final (rewind-replay reconciliation).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReplicationMode {
    /// Server simulates; clients render interpolated snapshots. The default.
    #[default]
    Authority,
    /// The owner-client ALSO simulates locally, ahead of the server — for the
    /// player's own avatar. Requires an `owner` peer.
    Predicted,
}

/// Marks a node as networked — the Inspector's "Networked" component and the
/// target of `node:replicate{...}` in Lua. Only nodes carrying this replicate;
/// everything else stays local by construction.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Replicated {
    /// How clients treat it (server-simulated vs owner-predicted).
    pub mode: ReplicationMode,
    /// Whose inputs drive it in `Predicted` mode (a session peer id).
    /// Meaningless when `mode` is `Authority`. Runtime state, not authored —
    /// assigned by the session (e.g. `net.spawn(..., { owner = peer })`).
    pub owner: Option<u64>,
    /// Sync position/rotation (+ interpolate on remote clients).
    pub transform: bool,
    /// Sync velocity too (better extrapolation, and required for prediction
    /// of a physics body).
    pub physics: bool,
    /// Smooth remote entities between snapshots (off = snap — for teleporty
    /// things where interpolation would look like motion).
    pub interp: bool,
    /// How far behind the newest server tick remote copies render, in gameplay
    /// ticks (default 6 ≈ 100 ms at 60 Hz). Lower = tighter tracking but
    /// stutters under jitter/loss; higher = smoother on bad links. Ignored
    /// when `interp` is off.
    pub interp_delay: u8,
}

impl Replicated {
    /// The default remote-render delay, gameplay ticks (~100 ms at 60 Hz).
    pub const DEFAULT_INTERP_DELAY: u8 = 6;
}

impl Default for Replicated {
    fn default() -> Self {
        Self {
            mode: ReplicationMode::Authority,
            owner: None,
            transform: true,
            physics: false,
            interp: true,
            interp_delay: Self::DEFAULT_INTERP_DELAY,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn net_id_assignment() {
        assert!(!NetId::UNASSIGNED.is_assigned());
        assert!(NetId(7).is_assigned());
        assert_eq!(NetId::default(), NetId::UNASSIGNED);
    }

    #[test]
    fn replicated_defaults_are_the_safe_ones() {
        let r = Replicated::default();
        assert_eq!(r.mode, ReplicationMode::Authority);
        assert!(r.transform && r.interp && !r.physics && r.owner.is_none());
    }
}
