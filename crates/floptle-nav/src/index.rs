//! Finding the polygon under a point without looking at all of them.
//!
//! Every query in this crate starts with "which polygon is this point on", and
//! it was a scan over every polygon in the level. That is fine for one character
//! and it is the wrong shape for a crowd: an RTS asking for two hundred units
//! against a four-thousand-polygon level is eight hundred thousand rectangle
//! tests **per frame**, and it gets worse as the level grows rather than as the
//! army does. That is the same silent-quadratic shape [`crate`]'s scaling guards
//! exist to catch, one layer up.
//!
//! So: a flat grid of buckets over the level in plan, each holding the polygons
//! that overlap it. A lookup touches the handful of buckets a query box covers
//! and the handful of polygons in them.
//!
//! # Why a grid and not a tree
//!
//! The polygons are already grid-derived rectangles spread evenly over the
//! walkable ground, which is the case a uniform grid is best at and a BVH is
//! only equal at. A grid also builds in one pass with two allocations, and it
//! has no rebalancing to get wrong when a bake changes.
//!
//! # Order is part of the answer
//!
//! Buckets are visited in order and a polygon can sit in several of them, so
//! callers must break ties by polygon index rather than by what they saw first.
//! [`NavMesh::nearest`](crate::NavMesh::nearest) does, and that is what keeps a
//! path the same on every run — the property the whole crate is careful about.

use crate::mesh::Poly;

/// A bucket grid over the polygons of one navmesh, in plan.
#[derive(Clone, Debug, Default)]
pub struct PolyIndex {
    origin: [f32; 2],
    bucket: f32,
    width: usize,
    depth: usize,
    /// Where each bucket's polygons start in `items`, with one extra on the end
    /// — the same CSR shape [`WalkableGrid`](crate::WalkableGrid) uses for
    /// columns, and for the same reason: one `Vec` per bucket is millions of
    /// allocations on a big level.
    start: Vec<u32>,
    items: Vec<u32>,
}

impl PolyIndex {
    /// Build an index over these polygons.
    ///
    /// `cell_size` is the bake's, and only sets a floor on how fine the buckets
    /// can get. The bucket size itself comes from the polygons: aim for a few
    /// per bucket, so a query touches a handful and the index is a fraction of
    /// the mesh it indexes.
    pub fn build(polys: &[Poly], cell_size: f32) -> PolyIndex {
        if polys.is_empty() {
            return PolyIndex::default();
        }
        let mut lo = [f32::INFINITY; 2];
        let mut hi = [f32::NEG_INFINITY; 2];
        for p in polys {
            for a in 0..2 {
                lo[a] = lo[a].min(p.min[a]);
                hi[a] = hi[a].max(p.max[a]);
            }
        }
        let span = (hi[0] - lo[0]).max(hi[1] - lo[1]).max(cell_size);
        // One bucket per polygon's worth of area, near enough: with `n`
        // polygons over a square level, `span / sqrt(n)` buckets a side puts
        // about one polygon in each. Floored at four cells so a level of tiny
        // fragments cannot ask for a bucket grid bigger than the heightfield
        // that made it.
        let bucket = (span / (polys.len() as f32).sqrt()).max(cell_size * 4.0);
        let width = (((hi[0] - lo[0]) / bucket).ceil() as usize + 1).max(1);
        let depth = (((hi[1] - lo[1]) / bucket).ceil() as usize + 1).max(1);

        // Counting pass, then a filling pass — the CSR build that needs no
        // per-bucket `Vec` at all.
        let mut start = vec![0u32; width * depth + 1];
        let mut spans: Vec<(usize, usize, usize, usize)> = Vec::with_capacity(polys.len());
        for p in polys {
            let (x0, z0) = bucket_of(p.min, lo, bucket, width, depth);
            let (x1, z1) = bucket_of(p.max, lo, bucket, width, depth);
            spans.push((x0, z0, x1, z1));
            for z in z0..=z1 {
                for x in x0..=x1 {
                    start[z * width + x + 1] += 1;
                }
            }
        }
        for i in 1..start.len() {
            start[i] += start[i - 1];
        }
        let mut fill = start.clone();
        let mut items = vec![0u32; *start.last().unwrap_or(&0) as usize];
        for (i, (x0, z0, x1, z1)) in spans.into_iter().enumerate() {
            for z in z0..=z1 {
                for x in x0..=x1 {
                    let b = z * width + x;
                    items[fill[b] as usize] = i as u32;
                    fill[b] += 1;
                }
            }
        }

        PolyIndex { origin: lo, bucket, width, depth, start, items }
    }

    /// Whether this index has anything in it. A mesh loaded from disk before its
    /// index is built answers `false`, and callers fall back to a scan rather
    /// than to a wrong answer.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Hand every polygon that might overlap a plan-space box to `f`.
    ///
    /// **Might**: buckets are coarse, so this over-reports and the caller still
    /// has to test the polygons it gets. It never under-reports, which is the
    /// half that would turn into a character standing on ground the navmesh
    /// insists is not there.
    ///
    /// A polygon spanning several buckets is offered once per bucket. Callers
    /// that cannot take a repeat should compare by index rather than count.
    pub fn for_each_in(&self, min: [f32; 2], max: [f32; 2], mut f: impl FnMut(usize)) {
        if self.items.is_empty() {
            return;
        }
        let (x0, z0) = bucket_of(min, self.origin, self.bucket, self.width, self.depth);
        let (x1, z1) = bucket_of(max, self.origin, self.bucket, self.width, self.depth);
        for z in z0..=z1 {
            let row = z * self.width;
            for x in x0..=x1 {
                let b = row + x;
                for &i in &self.items[self.start[b] as usize..self.start[b + 1] as usize] {
                    f(i as usize);
                }
            }
        }
    }
}

/// Which bucket a plan point falls in, clamped to the grid — a point outside the
/// level belongs to the edge bucket, so a query near the boundary still sees the
/// polygons at the boundary.
fn bucket_of(
    p: [f32; 2],
    origin: [f32; 2],
    bucket: f32,
    width: usize,
    depth: usize,
) -> (usize, usize) {
    let bx = ((p[0] - origin[0]) / bucket).floor();
    let bz = ((p[1] - origin[1]) / bucket).floor();
    // `clamp` sends ±∞ to the matching EDGE bucket — which is what makes an
    // unbounded query (`nearest` with `max_distance = math.huge` from Lua) span
    // the whole grid instead of collapsing into the corner bucket. NaN falls
    // out of the cast as 0, which is as good an answer as any for a NaN point.
    let x = bx.clamp(0.0, (width - 1) as f32) as usize;
    let z = bz.clamp(0.0, (depth - 1) as f32) as usize;
    (x, z)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn poly(x0: f32, z0: f32, w: f32, d: f32) -> Poly {
        Poly {
            x0: 0,
            z0: 0,
            w: 0,
            d: 0,
            region: 0,
            area: 0,
            min: [x0, z0],
            max: [x0 + w, z0 + d],
            y_min: 0.0,
            y_max: 0.0,
            centre: [x0 + w * 0.5, 0.0, z0 + d * 0.5],
        }
    }

    /// A grid of rectangles, and a box over one of them must not offer the whole
    /// level back — that is the entire point of the index existing.
    #[test]
    fn a_query_sees_its_own_neighbourhood_rather_than_the_level() {
        let polys: Vec<Poly> =
            (0..40).flat_map(|z| (0..40).map(move |x| poly(x as f32, z as f32, 1.0, 1.0))).collect();
        let index = PolyIndex::build(&polys, 0.25);

        let mut seen = Vec::new();
        index.for_each_in([10.2, 10.2], [10.8, 10.8], |i| seen.push(i));
        assert!(!seen.is_empty(), "the polygon under the point must be offered");
        assert!(
            seen.len() < polys.len() / 8,
            "a point query offered {} of {} polygons — that is a scan wearing a hat",
            seen.len(),
            polys.len()
        );
        assert!(
            seen.iter().any(|&i| polys[i].contains_xz([10.5, 0.0, 10.5])),
            "the polygon the point is actually over was missing: {seen:?}"
        );
    }

    /// Over-reporting is fine and under-reporting is a character falling through
    /// the floor, so the property worth pinning is that nothing overlapping is
    /// ever missed — checked against the scan it replaces.
    #[test]
    fn nothing_that_overlaps_is_ever_missed() {
        // Deliberately awkward: rectangles of wildly different sizes, including
        // one that spans the whole level, and coordinates that are not round.
        let mut polys = vec![poly(-3.25, -3.25, 60.5, 60.5)];
        for i in 0..200 {
            let f = i as f32;
            polys.push(poly(f * 0.37 - 2.0, f * 0.11, 0.4 + f * 0.05, 1.3));
        }
        let index = PolyIndex::build(&polys, 0.15);

        for k in 0..60 {
            let f = k as f32 * 0.9 - 5.0;
            let (min, max) = ([f, f * 0.5], [f + 2.2, f * 0.5 + 0.7]);
            let mut got: Vec<usize> = Vec::new();
            index.for_each_in(min, max, |i| got.push(i));
            got.sort_unstable();
            got.dedup();

            let want: Vec<usize> = polys
                .iter()
                .enumerate()
                .filter(|(_, p)| {
                    p.min[0] <= max[0]
                        && p.max[0] >= min[0]
                        && p.min[1] <= max[1]
                        && p.max[1] >= min[1]
                })
                .map(|(i, _)| i)
                .collect();
            for i in want {
                assert!(got.contains(&i), "polygon {i} overlaps the box at {f} and was not offered");
            }
        }
    }

    /// Lua hands `nearest`/`snap` whatever number the script wrote, and
    /// `math.huge` is the idiomatic "no limit" — an unbounded query box must
    /// span the whole grid, not collapse into the corner bucket.
    #[test]
    fn an_unbounded_query_spans_the_grid() {
        let polys: Vec<Poly> =
            (0..10).map(|x| poly(x as f32 * 5.0, 40.0, 1.0, 1.0)).collect();
        let index = PolyIndex::build(&polys, 0.25);
        let mut seen = Vec::new();
        index.for_each_in([f32::NEG_INFINITY; 2], [f32::INFINITY; 2], |i| seen.push(i));
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), polys.len(), "every polygon is offered to an unbounded query");
    }

    /// A mesh with no polygons is the ordinary state of a project that has not
    /// baked one, and it must answer rather than divide by zero.
    #[test]
    fn an_empty_mesh_indexes_to_nothing() {
        let index = PolyIndex::build(&[], 0.15);
        assert!(index.is_empty());
        index.for_each_in([0.0, 0.0], [1.0, 1.0], |_| panic!("there is nothing to offer"));
    }
}
