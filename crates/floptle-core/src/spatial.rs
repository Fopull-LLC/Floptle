//! A spatial index over things with a position and a radius (`floptle/0076`).
//!
//! Every system that answered *"what is near here?"* walked everything. That is
//! the structural reason behind a recurring class of bug rather than a
//! theoretical concern: `World::get` being a linear scan cost 60 ms/frame of pure
//! lookups at 5,500 nodes (`floptle/0059`), `findScript` was the same shape
//! (`floptle/0063`), and both were found by a player.
//!
//! # Why a hash grid, and not a BVH
//!
//! The card asked to measure before committing to a shape, so:
//!
//! * **The things being indexed MOVE.** These are physics bodies, queried by
//!   gameplay every frame. A BVH would need a refit per frame, and a refit over
//!   moving leaves costs more than rebuilding a grid — which is one pass, no
//!   comparisons, no tree.
//! * **The floating-origin rebase moves every item at once** (ADR-0015). A
//!   structure with cached bounds would have to be told; a grid rebuilt from this
//!   frame's positions, in the same frame the query uses, cannot go stale. That
//!   is not a small property — it is the difference between "survives the rebase"
//!   and "survives the rebase until someone forgets".
//! * **Build cost is the budget.** A grid build is O(n) with no allocation once
//!   the buffers are warm, because [`Grid::rebuild`] reuses them.
//!
//! # The oversized-item trap
//!
//! This engine has planet-sized colliders. An item whose radius spans thousands
//! of cells would, inserted honestly, fill the map and make every query slower
//! than the scan it replaced. So items bigger than [`Grid::OVERSIZED_CELLS`]
//! cells go into one always-returned list. That keeps the answer CORRECT (they
//! are still candidates for every query) while keeping the grid the size of the
//! things a grid is good for.
//!
//! Queries return **candidates**, not hits: the caller still does its own exact
//! test. That is what makes this safe to drop under an existing query — the
//! narrow phase is unchanged, so the answer cannot change, only the number of
//! things it is asked about.

use std::collections::HashMap;

use glam::Vec3;

/// One cell's integer coordinate.
type Cell = [i32; 3];

/// A uniform spatial hash over (centre, radius) items, keyed by their index in
/// whatever slice they came from.
#[derive(Clone, Debug, Default)]
pub struct Grid {
    /// Edge length of a cell, world units.
    cell: f32,
    cells: HashMap<Cell, Vec<u32>>,
    /// Items too big to insert without smearing across the whole map.
    oversized: Vec<u32>,
    /// How many items the last rebuild indexed (for [`Self::len`]).
    len: usize,
}

impl Grid {
    /// An item spanning more than this many cells per axis is treated as
    /// oversized and always considered a candidate.
    ///
    /// 8 is chosen so a large-but-ordinary body (a vehicle, a building) still
    /// indexes, while a planet does not try to occupy 10^6 cells.
    pub const OVERSIZED_CELLS: f32 = 8.0;

    /// Smallest cell edge. A degenerate cell size would put every item in one
    /// cell (slower than the scan) or hash astronomically many (worse).
    pub const MIN_CELL: f32 = 0.25;

    /// Rebuild the index from `items`, reusing the buffers from last time.
    ///
    /// The cell size comes from the DATA — a few times the mean radius — because
    /// a fixed size is wrong for both a room of crates and a solar system, and
    /// the mean is the one number that costs nothing to know while inserting.
    pub fn rebuild(&mut self, items: impl Iterator<Item = (Vec3, f32)> + Clone) {
        // Clear but keep the allocations: the per-cell Vecs are the expensive
        // part and a frame's occupancy barely changes.
        for v in self.cells.values_mut() {
            v.clear();
        }
        self.oversized.clear();
        let mut n = 0usize;
        let mut radius_sum = 0.0f32;
        for (_, r) in items.clone() {
            n += 1;
            radius_sum += r.max(0.0);
        }
        self.len = n;
        if n == 0 {
            self.cell = 1.0;
            return;
        }
        let mean = radius_sum / n as f32;
        self.cell = (mean * 4.0).max(Self::MIN_CELL);
        let limit = self.cell * Self::OVERSIZED_CELLS;
        for (i, (c, r)) in items.enumerate() {
            let i = i as u32;
            if !c.is_finite() || !r.is_finite() {
                // A NaN position hashes nowhere at all, which would silently
                // drop the item from every query. Oversized = always a
                // candidate, so the narrow phase still gets to reject it.
                self.oversized.push(i);
                continue;
            }
            if r > limit {
                self.oversized.push(i);
                continue;
            }
            let (lo, hi) = self.span(c, r);
            for x in lo[0]..=hi[0] {
                for y in lo[1]..=hi[1] {
                    for z in lo[2]..=hi[2] {
                        self.cells.entry([x, y, z]).or_default().push(i);
                    }
                }
            }
        }
        // A cell nobody occupies any more still costs a hash lookup on every
        // query that touches it, so drop the empties once they outnumber the
        // rest — amortised, not per frame.
        if self.cells.len() > n * 4 + 64 {
            self.cells.retain(|_, v| !v.is_empty());
        }
    }

    /// How many items the index holds.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The cell edge the last rebuild chose.
    pub fn cell_size(&self) -> f32 {
        self.cell
    }

    /// Candidate indices whose item may touch the sphere `(centre, radius)`.
    ///
    /// Pushes into `out` without clearing it, so a caller can accumulate from
    /// several queries. Candidates are unique.
    pub fn sphere(&self, centre: Vec3, radius: f32, out: &mut Vec<u32>) {
        out.extend_from_slice(&self.oversized);
        if self.cells.is_empty() || !centre.is_finite() || !radius.is_finite() {
            return;
        }
        let (lo, hi) = self.span(centre, radius);
        // A query wider than the whole index is cheaper answered by walking the
        // occupied cells than by iterating the volume it covers.
        let volume = (hi[0] - lo[0] + 1) as i64
            * (hi[1] - lo[1] + 1) as i64
            * (hi[2] - lo[2] + 1) as i64;
        if volume > self.cells.len() as i64 {
            for (c, v) in &self.cells {
                if c[0] >= lo[0]
                    && c[0] <= hi[0]
                    && c[1] >= lo[1]
                    && c[1] <= hi[1]
                    && c[2] >= lo[2]
                    && c[2] <= hi[2]
                {
                    out.extend_from_slice(v);
                }
            }
        } else {
            for x in lo[0]..=hi[0] {
                for y in lo[1]..=hi[1] {
                    for z in lo[2]..=hi[2] {
                        if let Some(v) = self.cells.get(&[x, y, z]) {
                            out.extend_from_slice(v);
                        }
                    }
                }
            }
        }
        // One item can sit in several cells, so a candidate can repeat. Sorting
        // here rather than making the caller do it keeps every narrow phase
        // exactly as it was.
        out.sort_unstable();
        out.dedup();
    }

    /// Candidate indices whose item may touch the axis-aligned box.
    ///
    /// Expressed as a sphere query over the box's bounding sphere: the narrow
    /// phase does the exact test anyway, and a second traversal would be a
    /// second thing to keep correct.
    pub fn aabb(&self, min: Vec3, max: Vec3, out: &mut Vec<u32>) {
        let centre = (min + max) * 0.5;
        let radius = (max - min).length() * 0.5;
        self.sphere(centre, radius, out);
    }

    /// Candidate indices for a swept sphere from `a` to `b` — the shape a
    /// `spherecast` broadphase needs.
    pub fn segment(&self, a: Vec3, b: Vec3, radius: f32, out: &mut Vec<u32>) {
        let centre = (a + b) * 0.5;
        let half = (b - a).length() * 0.5;
        self.sphere(centre, half + radius, out);
    }

    /// The inclusive cell range an item or query of `(centre, radius)` spans.
    fn span(&self, centre: Vec3, radius: f32) -> (Cell, Cell) {
        let inv = 1.0 / self.cell.max(Self::MIN_CELL);
        let lo = (centre - Vec3::splat(radius.max(0.0))) * inv;
        let hi = (centre + Vec3::splat(radius.max(0.0))) * inv;
        let f = |v: Vec3, up: bool| -> Cell {
            let g = |x: f32| -> i32 {
                let x = if up { x.ceil() } else { x.floor() };
                x.clamp(i32::MIN as f32 / 2.0, i32::MAX as f32 / 2.0) as i32
            };
            [g(v.x), g(v.y), g(v.z)]
        };
        (f(lo, false), f(hi, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(n: usize, spacing: f32, radius: f32) -> Vec<(Vec3, f32)> {
        // A cube-ish lattice, so a query touches a small fraction of it.
        let side = (n as f32).cbrt().ceil() as usize;
        (0..n)
            .map(|i| {
                let (x, y, z) = (i % side, (i / side) % side, i / (side * side));
                (
                    Vec3::new(x as f32 * spacing, y as f32 * spacing, z as f32 * spacing),
                    radius,
                )
            })
            .collect()
    }

    /// The index has to return every item the honest scan would — that is the
    /// only property that makes it safe to drop under an existing query.
    #[test]
    fn a_sphere_query_never_misses_what_the_scan_would_find() {
        let its = items(500, 2.0, 0.5);
        let mut g = Grid::default();
        g.rebuild(its.iter().copied());
        for probe in [
            (Vec3::new(0.0, 0.0, 0.0), 1.0),
            (Vec3::new(7.5, 3.0, 2.0), 4.0),
            (Vec3::new(-50.0, 0.0, 0.0), 3.0), // outside the lattice entirely
            (Vec3::new(8.0, 8.0, 8.0), 0.0),   // a point query
        ] {
            let mut cand = Vec::new();
            g.sphere(probe.0, probe.1, &mut cand);
            let truth: Vec<u32> = its
                .iter()
                .enumerate()
                .filter(|(_, (c, r))| c.distance(probe.0) <= r + probe.1)
                .map(|(i, _)| i as u32)
                .collect();
            for t in &truth {
                assert!(
                    cand.contains(t),
                    "the index missed item {t}, which the scan finds at {probe:?}"
                );
            }
        }
    }

    #[test]
    fn a_query_touches_a_small_fraction_of_a_big_index() {
        let its = items(4096, 2.0, 0.5);
        let mut g = Grid::default();
        g.rebuild(its.iter().copied());
        let mut cand = Vec::new();
        g.sphere(Vec3::new(10.0, 10.0, 10.0), 2.0, &mut cand);
        assert!(
            cand.len() < its.len() / 20,
            "a small query returned {} of {} items — the index is not narrowing \
             anything, which is the whole point",
            cand.len(),
            its.len()
        );
    }

    /// A planet-sized collider must not smear across the map.
    #[test]
    fn an_oversized_item_is_always_a_candidate_and_costs_one_entry() {
        let mut its = items(200, 2.0, 0.5);
        its.push((Vec3::ZERO, 100_000.0)); // a planet
        let planet = (its.len() - 1) as u32;
        let mut g = Grid::default();
        g.rebuild(its.iter().copied());
        // Far away from everything else, the planet is still offered.
        let mut cand = Vec::new();
        g.sphere(Vec3::new(9_000.0, 0.0, 0.0), 1.0, &mut cand);
        assert!(cand.contains(&planet), "an oversized item must never be missed");
        assert!(cand.len() < 5, "…and it must not drag the lattice with it: {cand:?}");
    }

    /// A rebase moves everything at once; the index is rebuilt from the same
    /// coordinates the query uses, so it cannot go stale (ADR-0015).
    #[test]
    fn the_index_survives_the_floating_origin_rebase() {
        let near = items(300, 2.0, 0.5);
        let shift = Vec3::splat(-4096.0);
        let far: Vec<(Vec3, f32)> = near.iter().map(|(c, r)| (*c + shift, *r)).collect();
        let mut g = Grid::default();
        g.rebuild(near.iter().copied());
        let mut before = Vec::new();
        g.sphere(Vec3::new(6.0, 6.0, 6.0), 3.0, &mut before);
        g.rebuild(far.iter().copied());
        let mut after = Vec::new();
        g.sphere(Vec3::new(6.0, 6.0, 6.0) + shift, 3.0, &mut after);
        assert_eq!(before, after, "the same query in the rebased frame must answer the same");
    }

    #[test]
    fn a_nan_position_is_still_offered_rather_than_vanishing() {
        // Silently dropping an item from every query is the failure shape this
        // engine keeps being bitten by (`floptle/0082`).
        let its = [(Vec3::new(f32::NAN, 0.0, 0.0), 1.0), (Vec3::ZERO, 1.0)];
        let mut g = Grid::default();
        g.rebuild(its.iter().copied());
        let mut cand = Vec::new();
        g.sphere(Vec3::ZERO, 1.0, &mut cand);
        assert!(cand.contains(&0), "a NaN item must reach the narrow phase, not disappear");
    }

    #[test]
    fn rebuilding_an_empty_index_is_not_a_division_by_zero() {
        let mut g = Grid::default();
        g.rebuild(std::iter::empty());
        assert!(g.is_empty());
        let mut cand = Vec::new();
        g.sphere(Vec3::ZERO, 5.0, &mut cand);
        assert!(cand.is_empty());
        assert!(g.cell_size() > 0.0, "a zero cell size would hash every query into one bucket");
    }

    #[test]
    fn a_segment_query_covers_the_whole_sweep() {
        let its = items(500, 2.0, 0.5);
        let mut g = Grid::default();
        g.rebuild(its.iter().copied());
        let (a, b) = (Vec3::new(0.0, 0.0, 0.0), Vec3::new(14.0, 0.0, 0.0));
        let mut cand = Vec::new();
        g.segment(a, b, 0.5, &mut cand);
        // Every lattice point along the x run has to be offered.
        for i in 0..=7u32 {
            assert!(cand.contains(&i), "the sweep missed item {i}");
        }
    }
}
