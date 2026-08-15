//! Ways across that are not ground: ladders, jump-downs, vaults, doors, teleports.
//!
//! A navmesh is a surface, and a surface can only say "walk from here to there
//! along the floor". Everything a character does that is *not* walking — dropping
//! off a ledge, climbing a ladder, stepping through a door onto a boat — is a
//! connection between two pieces of that surface with no floor in between. This
//! is that connection.
//!
//! A link is deliberately dumb: two points, a cost, and a switch. It does not
//! know what a ladder is. What makes it a ladder is the animation a script plays
//! while an agent is on it, and [`crate::Agent`] reports exactly that — *which*
//! link, and how far along it — for the whole of a traversal.
//!
//! # Cost is in metres
//!
//! A link's cost is what crossing it costs the search, measured in the same
//! units as walking, so `cost = 8.0` means "the router should treat this as an
//! eight-metre walk". A drop that is instant but risky and a ladder that is slow
//! and safe are both expressed the same way, and both are comparable against the
//! long way round — which is the only comparison the search can actually make.
//!
//! # A link whose end is nowhere
//!
//! An end that does not land on the navmesh leaves the link **unresolved**
//! rather than silently dropped. A door that quietly does nothing is the exact
//! failure shape this engine's audit found again and again: the level looks
//! right, the path goes the long way, and nothing anywhere says why.
//! [`NavMesh::unresolved_links`](crate::NavMesh::unresolved_links) is what the
//! editor reports after a bake.

use serde::{Deserialize, Serialize};

/// A link's end is not on the navmesh.
pub const NOWHERE: u32 = u32::MAX;

/// A connection between two places on the navmesh with no walking in between.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OffLink {
    /// The node's stable id, so a script can name this link and a rebake keeps
    /// meaning the same one.
    pub id: u32,
    /// What it is called in the scene — the name a script asks for, and the one
    /// an agent reports while it is crossing.
    pub name: String,
    /// Where a character steps on. Snapped onto the navmesh by the bake.
    pub from: [f32; 3],
    /// Where it steps off.
    pub to: [f32; 3],
    /// Whether it can be crossed the other way as well. A ladder is; a
    /// jump-down is not, which is the whole reason this is a switch.
    pub bidirectional: bool,
    /// What crossing costs the search, in metres of ordinary walking.
    pub cost: f32,
    /// Which area it counts as, so a filter can rule out every jump in the level
    /// with one exclusion rather than one per link.
    pub area: u8,
    /// How long a crossing takes, in seconds. `0` means "at walking speed",
    /// which is right for a vault and wrong for a lift.
    pub duration: f32,
    /// Off means the search cannot see it. Doors, drawbridges, a ladder that
    /// burns down in act two.
    pub enabled: bool,
    /// The polygon each end landed on, or [`NOWHERE`].
    pub from_poly: u32,
    pub to_poly: u32,
}

impl OffLink {
    /// A link between two world points, with everything else defaulted: one-way,
    /// free beyond the distance it covers, ordinary ground, on.
    pub fn new(id: u32, name: impl Into<String>, from: [f32; 3], to: [f32; 3]) -> OffLink {
        let cost = {
            let (dx, dy, dz) = (to[0] - from[0], to[1] - from[1], to[2] - from[2]);
            (dx * dx + dy * dy + dz * dz).sqrt()
        };
        OffLink {
            id,
            name: name.into(),
            from,
            to,
            bidirectional: false,
            cost,
            area: crate::WALKABLE,
            duration: 0.0,
            enabled: true,
            from_poly: NOWHERE,
            to_poly: NOWHERE,
        }
    }

    /// Both ends found ground to sit on.
    pub fn resolved(&self) -> bool {
        self.from_poly != NOWHERE && self.to_poly != NOWHERE
    }

    /// Whether the search may use it in the given direction right now.
    pub fn usable(&self, forwards: bool) -> bool {
        self.enabled && self.resolved() && (forwards || self.bidirectional)
    }

    /// Which end you arrive at, going this way.
    pub fn ends(&self, forwards: bool) -> ([f32; 3], [f32; 3]) {
        if forwards {
            (self.from, self.to)
        } else {
            (self.to, self.from)
        }
    }

    /// Which polygon you arrive on, going this way.
    pub fn target(&self, forwards: bool) -> u32 {
        if forwards {
            self.to_poly
        } else {
            self.from_poly
        }
    }

    /// How far apart its ends are.
    pub fn length(&self) -> f32 {
        let (dx, dy, dz) =
            (self.to[0] - self.from[0], self.to[1] - self.from[1], self.to[2] - self.from[2]);
        (dx * dx + dy * dy + dz * dz).sqrt()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_link_costs_what_it_spans() {
        let l = OffLink::new(1, "drop", [0.0, 4.0, 0.0], [3.0, 0.0, 0.0]);
        assert!((l.cost - 5.0).abs() < 1e-5, "3-4-5: {}", l.cost);
        assert!(!l.bidirectional, "a jump down is not a jump up");
        assert!(!l.resolved(), "nothing has told it where the ground is yet");
        assert!(!l.usable(true), "and an unresolved link must never be walked");
    }

    #[test]
    fn direction_decides_both_ends_and_the_arrival() {
        let mut l = OffLink::new(1, "ladder", [0.0, 0.0, 0.0], [0.0, 5.0, 0.0]);
        l.from_poly = 7;
        l.to_poly = 9;
        assert!(l.usable(true));
        assert!(!l.usable(false), "one-way until it is said to be two-way");
        l.bidirectional = true;
        assert!(l.usable(false));
        assert_eq!(l.target(false), 7);
        assert_eq!(l.ends(false), ([0.0, 5.0, 0.0], [0.0, 0.0, 0.0]));

        l.enabled = false;
        assert!(!l.usable(true), "a closed door is not a way through");
    }
}
