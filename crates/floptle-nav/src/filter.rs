//! Ground that costs more to cross, and characters that refuse to cross it.
//!
//! One navmesh, many characters: a guard takes the road, a zombie wades the
//! river, and neither of them should need their own bake to do it. That is two
//! separate ideas and this module holds both.
//!
//! **An area** is a label painted on the ground at bake time — *water*, *mud*,
//! *road*, *danger*. It belongs to the level, and it carries a default cost
//! because most of the time "mud is slow" is a fact about the mud rather than an
//! opinion held by one character.
//!
//! **A filter** is one character's reading of those labels: a multiplier per
//! area on top of the level's own, and a bit per area saying whether this
//! character will set foot in it at all. Filters live in the query, so two
//! characters standing on the same polygon can get different routes out of it.
//!
//! ```
//! # use floptle_nav::{Area, QueryFilter};
//! let areas = [Area::walkable(), Area::new("water", 4.0)];
//! // A guard will not swim.
//! let guard = QueryFilter::default().avoiding(1);
//! // A zombie will, and does not mind it much.
//! let zombie = QueryFilter::default().costing(1, 0.25);
//! assert!(!guard.passable(1));
//! assert!(zombie.passable(1));
//! assert!(zombie.cost(1, &areas) < guard.cost(1, &areas));
//! ```
//!
//! # Why thirty-two
//!
//! A filter's include/exclude set is one `u32`, so a bake can carry
//! [`MAX_AREAS`] kinds of ground. That is the number Recast settled on for the
//! same reason: it fits in a register, a query filter is copied per search, and
//! nobody has ever needed the thirty-third.

use serde::{Deserialize, Serialize};

/// How many kinds of ground one bake can tell apart.
pub const MAX_AREAS: usize = 32;

/// The area every polygon has unless something painted it otherwise.
pub const WALKABLE: u8 = 0;

/// One kind of ground, as the level describes it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Area {
    /// What a designer calls it. **The name is the identity** — a script says
    /// `"water"`, not `3`, so inserting a new area in the editor cannot quietly
    /// re-point every script at a different one.
    pub name: String,
    /// What a metre of it costs to cross, as a multiple of ordinary ground.
    /// Above 1 is avoided when there is a way round, below 1 is preferred.
    pub cost: f32,
}

impl Area {
    pub fn new(name: impl Into<String>, cost: f32) -> Area {
        Area { name: name.into(), cost: cost.max(0.0) }
    }

    /// Plain ground — area 0, which is what a bake with no volumes in it is made
    /// entirely of.
    pub fn walkable() -> Area {
        Area { name: "walkable".into(), cost: 1.0 }
    }
}

/// One character's rules for reading a navmesh.
///
/// Copy, and small enough that passing it by value per query is the cheap
/// option. Default is "everything is passable and costs what the level says".
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct QueryFilter {
    /// Multiplier per area, on top of the area's own cost.
    costs: [f32; MAX_AREAS],
    /// Bit per area: set means this character will walk on it.
    allowed: u32,
}

impl Default for QueryFilter {
    fn default() -> Self {
        QueryFilter { costs: [1.0; MAX_AREAS], allowed: u32::MAX }
    }
}

impl QueryFilter {
    /// Refuse to walk on an area at all.
    pub fn avoiding(mut self, area: u8) -> Self {
        self.exclude(area);
        self
    }

    /// Multiply what an area costs this character.
    pub fn costing(mut self, area: u8, multiplier: f32) -> Self {
        self.set_cost(area, multiplier);
        self
    }

    pub fn exclude(&mut self, area: u8) {
        if (area as usize) < MAX_AREAS {
            self.allowed &= !(1 << area);
        }
    }

    pub fn include(&mut self, area: u8) {
        if (area as usize) < MAX_AREAS {
            self.allowed |= 1 << area;
        }
    }

    pub fn set_cost(&mut self, area: u8, multiplier: f32) {
        if (area as usize) < MAX_AREAS {
            self.costs[area as usize] = multiplier.max(0.0);
        }
    }

    /// Will this character set foot on that area?
    ///
    /// An area beyond [`MAX_AREAS`] cannot be named by a filter, so it is
    /// passable — the safe direction: a bake that somehow carries an area
    /// nobody can express should not become a level nobody can cross.
    pub fn passable(&self, area: u8) -> bool {
        (area as usize) >= MAX_AREAS || self.allowed & (1 << area) != 0
    }

    /// What a metre of that area costs this character: the level's own cost
    /// times this filter's multiplier.
    pub fn cost(&self, area: u8, areas: &[Area]) -> f32 {
        let own = areas.get(area as usize).map(|a| a.cost).unwrap_or(1.0);
        let mine = self.costs.get(area as usize).copied().unwrap_or(1.0);
        (own * mine).max(0.0)
    }

    /// The cheapest any passable area can be.
    ///
    /// A\* only finds the shortest route while its estimate never overshoots the
    /// real remaining cost, and straight-line distance overshoots the moment
    /// some ground costs **less** than ordinary ground — a road at half price
    /// makes a two-metre gap cost one. Scaling the estimate by this keeps it
    /// honest, so a discount cannot quietly turn the search into a greedy one
    /// that returns a worse route than the one it walked past.
    pub fn cheapest(&self, areas: &[Area]) -> f32 {
        let mut best = f32::INFINITY;
        for a in 0..MAX_AREAS.min(areas.len().max(1)) {
            if self.passable(a as u8) {
                best = best.min(self.cost(a as u8, areas));
            }
        }
        if best.is_finite() {
            best.clamp(0.0, 1.0)
        } else {
            1.0
        }
    }
}

/// A box painted over the level at bake time: this ground is that area, or this
/// ground is not walkable at all.
///
/// Given as **the inverse of the volume's transform**, so the test is "put the
/// point in the box's own frame and see if it landed inside the unit cube". That
/// costs one matrix multiply per cell and it handles a rotated, scaled,
/// arbitrarily-parented volume without this crate having to know what any of
/// those words mean.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AreaVolume {
    /// World → the volume's own frame, column-major, where the volume is the
    /// cube from -1 to 1.
    pub inverse: [f32; 16],
    /// Which area to paint. Ignored when `blocks`.
    pub area: u8,
    /// Carve this ground out of the bake entirely, rather than labelling it.
    ///
    /// The designer's answer to "nothing may walk here" that does not involve
    /// building an invisible wall and remembering why it is there. It also
    /// cannot be defeated by a filter, which is the point: a filter is one
    /// character's opinion and this is a fact about the level.
    pub blocks: bool,
}

impl AreaVolume {
    /// Is this world point inside the box?
    pub fn contains(&self, p: [f32; 3]) -> bool {
        let m = &self.inverse;
        let q = [
            m[0] * p[0] + m[4] * p[1] + m[8] * p[2] + m[12],
            m[1] * p[0] + m[5] * p[1] + m[9] * p[2] + m[13],
            m[2] * p[0] + m[6] * p[1] + m[10] * p[2] + m[14],
        ];
        q.iter().all(|c| c.abs() <= 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filter_can_price_and_refuse_ground_independently() {
        let areas = [Area::walkable(), Area::new("mud", 3.0), Area::new("road", 0.5)];
        let f = QueryFilter::default();
        assert_eq!(f.cost(0, &areas), 1.0);
        assert_eq!(f.cost(1, &areas), 3.0, "the level's own cost applies with no filter");
        assert!(f.passable(1));

        // One character finds mud twice as bad again, and will not touch roads.
        let picky = QueryFilter::default().costing(1, 2.0).avoiding(2);
        assert_eq!(picky.cost(1, &areas), 6.0);
        assert!(!picky.passable(2));
        assert!(picky.passable(0), "excluding one area must not exclude the rest");
    }

    /// The estimate has to stay under the truth or A\* stops being A\*. Discounted
    /// ground is the case that breaks it, so the discount is what the estimate is
    /// scaled by.
    #[test]
    fn a_discount_is_carried_into_the_estimate() {
        let areas = [Area::walkable(), Area::new("road", 0.4)];
        assert!((QueryFilter::default().cheapest(&areas) - 0.4).abs() < 1e-6);

        // Refuse the road and the discount is gone with it.
        let no_road = QueryFilter::default().avoiding(1);
        assert!((no_road.cheapest(&areas) - 1.0).abs() < 1e-6);

        // Nothing cheap about a level of expensive ground — but the estimate
        // must never be scaled UP, or it overshoots in the other direction.
        let dear = [Area::walkable(), Area::new("mud", 9.0)];
        assert!((QueryFilter::default().cheapest(&dear) - 1.0).abs() < 1e-6);
    }

    /// A rotated volume is the ordinary case — a designer drags a box and turns
    /// it — and the whole reason this takes a matrix rather than two corners.
    #[test]
    fn a_turned_box_still_knows_what_is_inside_it() {
        // A 2x2x2 box at the origin, turned 45° about y. Its own frame is the
        // unit cube, so the inverse of a rotation is its transpose.
        let c = std::f32::consts::FRAC_1_SQRT_2;
        let inverse = [c, 0.0, -c, 0.0, 0.0, 1.0, 0.0, 0.0, c, 0.0, c, 0.0, 0.0, 0.0, 0.0, 1.0];
        let v = AreaVolume { inverse, area: 1, blocks: false };
        assert!(v.contains([0.0, 0.0, 0.0]));
        // 1.3 along world x is inside a box turned to face that way, and would
        // be outside the same box left square — which is the whole difference
        // this is here to prove.
        assert!(v.contains([1.3, 0.0, 0.0]));
        // …and its own corner direction runs out sooner than an axis does.
        assert!(!v.contains([1.3, 0.0, 1.3]), "that is 1.8 along the box's own axis");
        assert!(!v.contains([0.0, 1.4, 0.0]), "and up is up whichever way it is turned");
    }
}
