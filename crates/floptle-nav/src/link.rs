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

/// What a link is, and — the part that matters — who put it there.
///
/// A level's ladders and doors are placed by hand and are nobody's business but
/// the designer's. A drop off a ledge and a jump across a gap are neither: they
/// are facts about the shape of the floor, there are hundreds of them, and
/// asking somebody to place each one is asking them not to. The bake works those
/// out itself, and this is how everything downstream tells the two apart —
/// which one to draw differently, which ones a character that cannot jump must
/// refuse, and which ones a rebake is free to throw away and make again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LinkKind {
    /// Put here by a person: a ladder, a door, a lift, a rope, a teleport. The
    /// bake never invents one of these and never removes one.
    #[default]
    Placed,
    /// Worked out by the bake: off a ledge onto ground below. **One-way**,
    /// because falling is.
    Drop,
    /// Worked out by the bake: across a gap onto ground at about the same
    /// height. **Two-way**, because a gap you can clear you can clear coming
    /// back.
    Jump,
}

impl LinkKind {
    /// Whether the bake made this one, and would make it again.
    pub fn generated(self) -> bool {
        !matches!(self, LinkKind::Placed)
    }

    /// The name a script and the Inspector use.
    pub fn as_str(self) -> &'static str {
        match self {
            LinkKind::Placed => "placed",
            LinkKind::Drop => "drop",
            LinkKind::Jump => "jump",
        }
    }
}

/// Where a crossing is at `t`, running 0 at the mouth to 1 at the landing.
///
/// **The shape is the kind of crossing.** A drop leaves the ledge flat and
/// accelerates downward, which is what falling does; a jump bows over its gap; a
/// ladder, a lift and everything else somebody placed goes straight, because a
/// ladder that arcs is a ladder nobody built.
///
/// One function, used by both the editor's overlay and the crowd that carries an
/// agent across. Two copies of a curve is two curves, and the one thing this
/// engine's overlay is for is showing what a character will actually do.
pub fn arc_point(kind: LinkKind, from: [f32; 3], to: [f32; 3], t: f32) -> [f32; 3] {
    let t = t.clamp(0.0, 1.0);
    let lerp = |a: f32, b: f32| a + (b - a) * t;
    let y = match kind {
        LinkKind::Drop => from[1] + (to[1] - from[1]) * t * t,
        LinkKind::Jump => {
            // Bowed in proportion to the crossing rather than by a constant
            // that looks right at one scale and silly at every other, and
            // capped so a long link does not launch anybody into orbit.
            let span = {
                let (dx, dy, dz) = (to[0] - from[0], to[1] - from[1], to[2] - from[2]);
                (dx * dx + dy * dy + dz * dz).sqrt()
            };
            lerp(from[1], to[1]) + (span * 0.22).min(1.0) * 4.0 * t * (1.0 - t)
        }
        LinkKind::Placed => lerp(from[1], to[1]),
    };
    [lerp(from[0], to[0]), y, lerp(from[2], to[2])]
}

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
    /// A ladder somebody placed, or a drop the bake found. See [`LinkKind`].
    ///
    /// `serde(default)` so a link read back out of an older shape is a placed
    /// one, which is what every link was before the bake could find its own.
    #[serde(default)]
    pub kind: LinkKind,
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
            kind: LinkKind::Placed,
        }
    }

    /// The same link, said to be of a kind. What the bake's own link finder
    /// uses; hand-placed links never call it.
    pub fn of_kind(mut self, kind: LinkKind) -> OffLink {
        self.kind = kind;
        self
    }

    /// Whether the bake made this one.
    pub fn generated(&self) -> bool {
        self.kind.generated()
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

    /// Where a crossing of this link is at `t` — see [`arc_point`].
    pub fn point_at(&self, t: f32) -> [f32; 3] {
        arc_point(self.kind, self.from, self.to, t)
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

    /// The shape of a crossing is the kind of crossing — and the overlay and the
    /// crowd both read it here, so a picture of a fall and a fall are the same
    /// curve.
    #[test]
    fn the_arc_falls_for_a_drop_bows_for_a_jump_and_is_straight_for_a_ladder() {
        let (high, low) = ([0.0, 4.0, 0.0], [2.0, 0.0, 0.0]);

        // A drop hangs on at the lip and accelerates down: halfway ACROSS is
        // still well above halfway DOWN. A straight line would be exactly 2.0,
        // and that is the invisible ramp this replaced.
        let mid = arc_point(LinkKind::Drop, high, low, 0.5);
        assert!(mid[1] > 2.5, "halfway across a drop is not halfway down: {}", mid[1]);
        assert!((mid[0] - 1.0).abs() < 1e-5, "it still moves across at a steady rate");

        // A jump bows above both of its ends.
        let (a, b) = ([0.0, 0.0, 0.0], [4.0, 0.0, 0.0]);
        let over = arc_point(LinkKind::Jump, a, b, 0.5);
        assert!(over[1] > 0.1, "a jump goes over the gap: {}", over[1]);

        // A ladder does not arc.
        let rung = arc_point(LinkKind::Placed, high, low, 0.5);
        assert!((rung[1] - 2.0).abs() < 1e-5, "a placed link is a straight line: {}", rung[1]);

        // Every kind starts and ends exactly where it says, or an agent
        // teleports on the first and last frame of every crossing.
        for kind in [LinkKind::Drop, LinkKind::Jump, LinkKind::Placed] {
            assert_eq!(arc_point(kind, high, low, 0.0), high, "{kind:?}");
            assert_eq!(arc_point(kind, high, low, 1.0), low, "{kind:?}");
            // …and `t` outside 0..1 is clamped rather than extrapolated into
            // the ground.
            assert_eq!(arc_point(kind, high, low, -3.0), high, "{kind:?}");
            assert_eq!(arc_point(kind, high, low, 9.0), low, "{kind:?}");
        }
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
