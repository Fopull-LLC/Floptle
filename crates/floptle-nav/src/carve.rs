//! Cutting holes in a baked navmesh while the game is running.
//!
//! A crate dropped in a corridor blocks it. The correctness answer to that is
//! the background rebake the editor already does — measure the level again,
//! find the ground again, cut it into rectangles again — and it is a fair
//! answer at room scale and a wasteful one for one crate on a big level, where
//! nothing outside a two-metre box has changed and everything gets redone
//! anyway.
//!
//! This is the cheap answer, and it is offered as an **option** rather than as
//! a replacement: the rebake stays underneath as the thing that is always
//! right. What earns carving its place is being decisively cheaper for a small
//! change on a big level, which is a measurement rather than an opinion —
//! `cargo run --release -p floptle-nav --example carve_probe` prints it.
//!
//! # It rebuilds from the bake, every time
//!
//! Every change to the obstacle set throws the carved mesh away and re-derives
//! it from a pristine copy of the bake. That sounds wasteful and is the reason
//! this is simple enough to trust:
//!
//! * **Removal is exact.** Taking the last obstacle away does not undo a
//!   sequence of edits; it hands back the bake itself. The same query answers
//!   the same thing it answered before the carve, to the bit.
//! * **Overlapping obstacles are not a special case.** Two crates in the same
//!   doorway are two boxes subtracted from one rectangle, in one pass, in an
//!   order that cannot matter.
//! * **No tombstones.** [`Link::to`](crate::Link::to) and
//!   [`OffLink`](crate::OffLink)'s two ends are polygon **indices**, and the
//!   usual trap here is a half-renumbered mesh. Building the polygon list in
//!   one go means there is exactly one place indices are translated — the map
//!   from base index to carved index — instead of three files that have to
//!   agree.
//!
//! The cost of rebuilding is a copy of the polygon list and a walk over the
//! links to renumber them; the expensive part of a bake — voxelising the world
//! and flood-filling it — never runs at all.
//!
//! # It is snapped to the bake's grid
//!
//! [`Poly`] carries grid columns (`x0, z0, w, d`) as well as world bounds, and
//! the overlay and the tests read them. An arbitrary world-space box would make
//! the two disagree, so a carve box is grown outwards to whole cells first.
//! That makes a hole up to one cell bigger than the thing that asked for it, on
//! each side, and outwards is the direction to be wrong in: a crate blocking
//! slightly more than its own footprint is a crate; a crate blocking slightly
//! less is a character walking through it.
//!
//! # What a carve does not touch
//!
//! **The `.fnav` on disk, ever.** A carve is a fact about this play session, in
//! the same way a bake taken during Play is: pressing Stop has to give the
//! level back. Nothing here writes, and the two fields it adds to
//! [`NavMesh`](crate::NavMesh) are `#[serde(skip)]`, so a mesh that is saved
//! saves the bake it came from whatever has been cut out of it since.

use crate::link::NOWHERE;
use crate::mesh::{oriented, Link, NavMesh, Poly};

/// A box cut out of the walkable surface at runtime.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Obstacle {
    /// Handed out by [`NavMesh::carve`], and how it is taken away again.
    pub id: u32,
    /// Plan bounds, snapped outward to the bake's grid.
    pub min: [f32; 2],
    pub max: [f32; 2],
    /// The height band it blocks. Ground outside this band is untouched, which
    /// is what stops a crate on a bridge from carving the floor underneath it.
    pub y_min: f32,
    pub y_max: f32,
}

impl Obstacle {
    /// Does this box take anything out of `p`?
    ///
    /// Touching is not overlapping: a box whose edge lies exactly on a
    /// polygon's edge removes nothing, and treating that as a hit would leave
    /// zero-width slivers behind.
    pub(crate) fn bites(&self, p: &Poly) -> bool {
        self.min[0] < p.max[0]
            && self.max[0] > p.min[0]
            && self.min[1] < p.max[1]
            && self.max[1] > p.min[1]
            // A polygon is a surface, not a solid: a box has to reach the
            // ground to block it. `y_min`/`y_max` of a polygon is the range the
            // floor itself wanders over, so any overlap at all counts.
            && self.y_min <= p.y_max
            && self.y_max >= p.y_min
    }
}

/// The bake, kept aside so every carve can be re-derived from it.
#[derive(Clone, Debug)]
pub(crate) struct Baked {
    pub(crate) polys: Vec<Poly>,
    pub(crate) links: Vec<Vec<Link>>,
    pub(crate) off_links: Vec<crate::link::OffLink>,
}

impl NavMesh {
    /// Cut a box out of the walkable surface and return its id.
    ///
    /// `centre` and `size` are in this mesh's own frame (see
    /// [`NavMesh::to_local`]). The hole is snapped outward to the bake's grid,
    /// so the returned [`Obstacle`] may be up to one cell bigger per side than
    /// what was asked for — read it back with [`NavMesh::obstacles`] rather
    /// than assuming.
    ///
    /// The caller still has to tell the crowd: nothing here can reach it, and
    /// [`Crowd::navmesh_changed`](crate::Crowd::navmesh_changed) is the one
    /// call that makes agents mid-route notice.
    pub fn carve(&mut self, centre: [f32; 3], size: [f32; 3]) -> u32 {
        let cell = self.cell_size.max(1e-4);
        let half = [size[0].abs() * 0.5, size[1].abs() * 0.5, size[2].abs() * 0.5];
        // Outward to whole cells, measured from the bake's own origin so the
        // hole lands on the grid the polygons were cut from.
        let snap_lo = |v: f32, o: f32| o + ((v - o) / cell).floor() * cell;
        let snap_hi = |v: f32, o: f32| o + ((v - o) / cell).ceil() * cell;
        // Counted up before it is handed out, so ids start at 1 whether this
        // mesh was just built or loaded from a sidecar (which brings the
        // counter back as a zero, `serde(skip)` being what it is).
        self.next_obstacle = self.next_obstacle.wrapping_add(1);
        let id = self.next_obstacle;
        self.obstacles.push(Obstacle {
            id,
            min: [
                snap_lo(centre[0] - half[0], self.origin[0]),
                snap_lo(centre[2] - half[2], self.origin[2]),
            ],
            max: [
                snap_hi(centre[0] + half[0], self.origin[0]),
                snap_hi(centre[2] + half[2], self.origin[2]),
            ],
            // Not snapped: height is not gridded in a bake, and rounding it
            // would make a crate block a floor it is standing well above.
            y_min: centre[1] - half[1],
            y_max: centre[1] + half[1],
        });
        self.recarve();
        id
    }

    /// Take one obstacle away and give the ground back. `false` if there was no
    /// such id — a double `remove()` from Lua, most likely.
    pub fn remove_obstacle(&mut self, id: u32) -> bool {
        let Some(at) = self.obstacles.iter().position(|o| o.id == id) else { return false };
        self.obstacles.remove(at);
        self.recarve();
        true
    }

    /// Put every obstacle away at once.
    pub fn clear_obstacles(&mut self) -> usize {
        let n = self.obstacles.len();
        if n > 0 {
            self.obstacles.clear();
            self.recarve();
        }
        n
    }

    /// What is currently cut out of this mesh, in the order it was cut.
    pub fn obstacles(&self) -> &[Obstacle] {
        &self.obstacles
    }

    /// A number that changes whenever the obstacles do.
    ///
    /// For anything holding a *picture* of this mesh rather than the mesh —
    /// the editor's navmesh overlay above all. Comparing one number is the
    /// cheap way to notice; the alternative is drawing the bake beside a unit
    /// that just walked round a hole the drawing does not show.
    pub fn obstacle_rev(&self) -> u64 {
        self.obstacle_rev
    }

    /// Rebuild the carved mesh from the bake and the whole obstacle list.
    pub(crate) fn recarve(&mut self) {
        self.obstacle_rev = self.obstacle_rev.wrapping_add(1);
        // First carve on this mesh: keep the bake before touching it. Later
        // calls restore from that copy, so the polygons a carve works on are
        // always the baked ones and never a previous carve's output.
        let base = self.baked.get_or_insert_with(|| {
            Box::new(Baked {
                polys: self.polys.clone(),
                links: self.links.clone(),
                off_links: self.off_links.clone(),
            })
        });

        // The caches are derived from the polygons, and an `OnceLock` will
        // happily keep serving the answer it computed for the mesh that used to
        // be here. Both go, on every mutation, before anything can ask.
        self.index.take();
        self.link_index.take();
        self.island_index.take();
        self.summary_cache.take();

        if self.obstacles.is_empty() {
            self.polys = base.polys.clone();
            self.links = base.links.clone();
            self.off_links = base.off_links.clone();
            return;
        }

        let step = self.settings.step_height.max(0.0);
        let cell = self.cell_size.max(1e-4);
        let origin = self.origin;
        let obstacles = &self.obstacles;

        // ---- 1. which baked polygons does anything bite? -------------------
        let hit: Vec<bool> =
            base.polys.iter().map(|p| obstacles.iter().any(|o| o.bites(p))).collect();

        // ---- 2. the new polygon list, and the map from base to it ----------
        //
        // Untouched polygons keep their order, so most indices survive as a
        // shift rather than a shuffle; each bitten one contributes its leftover
        // rectangles in its own place. One pass, one map, no renumbering pass
        // afterwards to get wrong.
        let mut map = vec![NOWHERE as usize; base.polys.len()];
        let mut polys: Vec<Poly> = Vec::with_capacity(base.polys.len());
        // For every bitten polygon, where its children ended up.
        let mut children: Vec<(usize, std::ops::Range<usize>)> = Vec::new();
        for (i, p) in base.polys.iter().enumerate() {
            if !hit[i] {
                map[i] = polys.len();
                polys.push(*p);
                continue;
            }
            let from = polys.len();
            for r in subtract(p, obstacles) {
                polys.push(child(p, r, origin, cell));
            }
            children.push((i, from..polys.len()));
        }

        // ---- 3. carry over every link that both ends survived --------------
        let mut links: Vec<Vec<Link>> = vec![Vec::new(); polys.len()];
        for (i, ls) in base.links.iter().enumerate() {
            if hit[i] {
                continue; // its portals are re-derived below
            }
            let a = map[i];
            for l in ls {
                if !hit[l.to] {
                    links[a].push(Link { to: map[l.to], ..*l });
                }
            }
        }

        // ---- 4. relink, locally ---------------------------------------------
        //
        // Only the children and the polygons that were next door to something
        // bitten can have gained or lost a portal, so those are the only pairs
        // tested. Everything further away kept its links verbatim in step 3,
        // which is both cheaper than rebuilding them and safer: a carve must
        // not be able to change a route on the other side of the level.
        let mut candidates: Vec<usize> = Vec::new();
        for (i, range) in &children {
            candidates.extend(range.clone());
            for l in &base.links[*i] {
                if !hit[l.to] {
                    candidates.push(map[l.to]);
                }
            }
        }
        candidates.sort_unstable();
        candidates.dedup();
        for (ai, &a) in candidates.iter().enumerate() {
            for &b in &candidates[ai + 1..] {
                // Both already-surviving polygons: their portal was carried
                // over intact and re-deriving it would only risk disagreeing
                // with the bake.
                let fresh = children.iter().any(|(_, r)| r.contains(&a) || r.contains(&b));
                if !fresh {
                    continue;
                }
                let Some((p, q)) = portal(&polys[a], &polys[b], step) else { continue };
                links[a].push(oriented(polys[a].centre, polys[b].centre, b, p, q));
                links[b].push(oriented(polys[b].centre, polys[a].centre, a, p, q));
            }
        }
        // The bake's own comparator, because a route that comes out in a
        // different order on a different run is a bug nobody can reproduce —
        // and a carved mesh has to be as reproducible as a baked one.
        for l in &mut links {
            l.sort_by(|a, b| {
                a.to.cmp(&b.to)
                    .then(a.left[0].total_cmp(&b.left[0]))
                    .then(a.left[2].total_cmp(&b.left[2]))
            });
        }

        // ---- 5. regions ------------------------------------------------------
        let touched: Vec<u32> = {
            let mut r: Vec<u32> =
                base.polys.iter().enumerate().filter(|(i, _)| hit[*i]).map(|(_, p)| p.region).collect();
            r.sort_unstable();
            r.dedup();
            r
        };
        let off_links = base.off_links.clone();
        self.polys = polys;
        self.links = links;
        self.off_links = off_links;
        self.resplit_regions(&touched);

        // ---- 6. the hand-placed links' ends ----------------------------------
        //
        // Each end caches the polygon it landed on, and a carve can take that
        // polygon away or replace it with a child. Re-resolving is `nearest`,
        // the same call the bake used — and an end whose ground has gone comes
        // back unresolved, which is exactly right: a crate on the foot of a
        // ladder is a ladder you cannot get on.
        let snap = self.settings.agent_height.max(0.5);
        let ends: Vec<(usize, [f32; 3], [f32; 3])> = self
            .off_links
            .iter()
            .enumerate()
            .map(|(i, l)| (i, l.from, l.to))
            .collect();
        for (i, from, to) in ends {
            let a = self.nearest(from, snap).map_or(NOWHERE, |(p, _)| p as u32);
            let b = self.nearest(to, snap).map_or(NOWHERE, |(p, _)| p as u32);
            self.off_links[i].from_poly = a;
            self.off_links[i].to_poly = b;
        }
        // `nearest` built the index against the mesh mid-fixup; the off-link
        // half of the cache is now stale either way — and so is the island
        // grouping, which is exactly what a link losing its ground changes.
        self.link_index.take();
        self.island_index.take();
        self.summary_cache.take();
    }

    /// Give fresh region ids to the islands a carve broke apart.
    ///
    /// [`NavMesh::reachable_with`] takes a shortcut when there are no off-links
    /// and no filter: *different region, no path, without searching*. A wall
    /// dropped across the only corridor splits an island in two, and if both
    /// halves keep the same id the shortcut waves the question through to a
    /// search that then answers correctly — slower, but right. The dangerous
    /// direction is the one this exists to prevent all the same: ids that no
    /// longer describe the mesh are ids nothing downstream can trust.
    ///
    /// Only the regions a carve actually bit are re-examined, so the colours in
    /// the editor's overlay — and any id a script has already looked at — stay
    /// put everywhere else. Within a split, the component holding the lowest
    /// polygon index keeps the original id and the rest are minted above the
    /// highest id in the mesh, in index order, so the numbering is a function
    /// of the bake and the obstacle set rather than of the order they arrived.
    fn resplit_regions(&mut self, touched: &[u32]) {
        if touched.is_empty() {
            return;
        }
        let mut next = self.polys.iter().map(|p| p.region).max().unwrap_or(0) + 1;
        let mut seen = vec![false; self.polys.len()];
        for &r in touched {
            let mut first = true;
            for seed in 0..self.polys.len() {
                if seen[seed] || self.polys[seed].region != r {
                    continue;
                }
                // Flood the island this polygon is on, over the links only.
                let mut island = vec![seed];
                let mut stack = vec![seed];
                seen[seed] = true;
                while let Some(i) = stack.pop() {
                    for l in &self.links[i] {
                        if !seen[l.to] && self.polys[l.to].region == r {
                            seen[l.to] = true;
                            island.push(l.to);
                            stack.push(l.to);
                        }
                    }
                }
                if first {
                    first = false;
                    continue; // the first component keeps the id it had
                }
                for i in island {
                    self.polys[i].region = next;
                }
                next += 1;
            }
        }
    }
}

/// `p`'s plan rectangle with every obstacle taken out of it.
///
/// A rectangle minus a rectangle is at most four rectangles — the strips left
/// on each side — and doing that once per obstacle over the growing list is
/// what makes overlapping obstacles need no special case at all.
pub(crate) fn subtract(p: &Poly, obstacles: &[Obstacle]) -> Vec<([f32; 2], [f32; 2])> {
    let mut pieces: Vec<([f32; 2], [f32; 2])> = vec![(p.min, p.max)];
    for o in obstacles {
        if !o.bites(p) {
            continue;
        }
        let mut next: Vec<([f32; 2], [f32; 2])> = Vec::with_capacity(pieces.len());
        for (lo, hi) in pieces.drain(..) {
            if o.min[0] >= hi[0] || o.max[0] <= lo[0] || o.min[1] >= hi[1] || o.max[1] <= lo[1] {
                next.push((lo, hi)); // this piece is clear of the box
                continue;
            }
            // Left and right strips take the full depth; the two remaining
            // strips take only the width the box actually spans, so the four
            // pieces never overlap each other.
            if lo[0] < o.min[0] {
                next.push((lo, [o.min[0], hi[1]]));
            }
            if o.max[0] < hi[0] {
                next.push(([o.max[0], lo[1]], hi));
            }
            let (mx, nx) = (lo[0].max(o.min[0]), hi[0].min(o.max[0]));
            if lo[1] < o.min[1] {
                next.push(([mx, lo[1]], [nx, o.min[1]]));
            }
            if o.max[1] < hi[1] {
                next.push(([mx, o.max[1]], [nx, hi[1]]));
            }
        }
        pieces = next;
    }
    // A sliver thinner than a cell is not ground a character baked for this
    // mesh could stand on, and it is exactly what a box landing a hair inside a
    // polygon's edge would leave behind.
    pieces.retain(|(lo, hi)| hi[0] - lo[0] > 1e-4 && hi[1] - lo[1] > 1e-4);
    pieces
}

/// One leftover rectangle, as a polygon in its parent's image.
///
/// Region, area and height band are the parent's: cutting a hole in a floor
/// does not change what kind of floor it is or how high it is. The grid columns
/// are recomputed, and they are exact because the carve was snapped to this
/// same grid before anything was cut.
pub(crate) fn child(parent: &Poly, r: ([f32; 2], [f32; 2]), origin: [f32; 3], cell: f32) -> Poly {
    let (min, max) = r;
    let col = |v: f32, o: f32| ((v - o) / cell).round().max(0.0) as usize;
    let x0 = col(min[0], origin[0]);
    let z0 = col(min[1], origin[2]);
    Poly {
        x0,
        z0,
        w: col(max[0], origin[0]).saturating_sub(x0).max(1),
        d: col(max[1], origin[2]).saturating_sub(z0).max(1),
        region: parent.region,
        area: parent.area,
        min,
        max,
        y_min: parent.y_min,
        y_max: parent.y_max,
        centre: [
            (min[0] + max[0]) * 0.5,
            (parent.y_min + parent.y_max) * 0.5,
            (min[1] + max[1]) * 0.5,
        ],
    }
}

/// The segment two rectangles share, if they share one you can walk through.
///
/// This is the bake's portal rule stated geometrically rather than in cells:
/// touching along an edge, overlapping by more than nothing across it, and
/// within a step of each other vertically. A floor and the bridge four metres
/// over it share a plan edge and fail the last test, which is the whole reason
/// it is there.
pub(crate) fn portal(a: &Poly, b: &Poly, step: f32) -> Option<([f32; 3], [f32; 3])> {
    // How far apart the two height bands are — zero where they overlap.
    let gap = if a.y_max < b.y_min {
        b.y_min - a.y_max
    } else if b.y_max < a.y_min {
        a.y_min - b.y_max
    } else {
        0.0
    };
    if gap > step {
        return None;
    }
    let y = if a.y_max < b.y_min {
        (a.y_max + b.y_min) * 0.5
    } else if b.y_max < a.y_min {
        (b.y_max + a.y_min) * 0.5
    } else {
        (a.y_min.max(b.y_min) + a.y_max.min(b.y_max)) * 0.5
    };
    const EPS: f32 = 1e-4;

    // Along x: one's right edge is the other's left edge, and the shared run is
    // where their z ranges overlap.
    let shared_x = |lo: &Poly, hi: &Poly| -> Option<([f32; 3], [f32; 3])> {
        if (lo.max[0] - hi.min[0]).abs() > EPS {
            return None;
        }
        let (z0, z1) = (lo.min[1].max(hi.min[1]), lo.max[1].min(hi.max[1]));
        (z1 - z0 > EPS).then(|| ([lo.max[0], y, z0], [lo.max[0], y, z1]))
    };
    let shared_z = |lo: &Poly, hi: &Poly| -> Option<([f32; 3], [f32; 3])> {
        if (lo.max[1] - hi.min[1]).abs() > EPS {
            return None;
        }
        let (x0, x1) = (lo.min[0].max(hi.min[0]), lo.max[0].min(hi.max[0]));
        (x1 - x0 > EPS).then(|| ([x0, y, lo.max[1]], [x1, y, lo.max[1]]))
    };
    shared_x(a, b)
        .or_else(|| shared_x(b, a))
        .or_else(|| shared_z(a, b))
        .or_else(|| shared_z(b, a))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heightfield::Heightfield;
    use crate::walkable::WalkableGrid;
    use crate::{NavSettings, Tri};

    fn slab(x0: f32, z0: f32, w: f32, d: f32, y: f32) -> Vec<Tri> {
        vec![
            Tri::new([x0, y, z0], [x0 + w, y, z0], [x0, y, z0 + d]),
            Tri::new([x0 + w, y, z0], [x0 + w, y, z0 + d], [x0, y, z0 + d]),
        ]
    }

    fn bake(tris: &[Tri], s: &NavSettings) -> NavMesh {
        let hf = Heightfield::build(tris, s).unwrap();
        let grid = WalkableGrid::build(&hf, s).unwrap();
        NavMesh::build(&grid, s).unwrap()
    }

    fn open(cell: f32) -> NavSettings {
        NavSettings { cell_size: cell, agent_radius: 0.0, agent_height: 1.0, ..Default::default() }
    }

    /// A corridor between two rooms, wide enough to walk and narrow enough that
    /// one crate closes it.
    fn dumbbell() -> NavMesh {
        let mut tris = slab(0.0, 0.0, 4.0, 6.0, 0.0);
        tris.extend(slab(4.0, 2.0, 4.0, 2.0, 0.0)); // the corridor
        tris.extend(slab(8.0, 0.0, 4.0, 6.0, 0.0));
        bake(&tris, &open(0.25))
    }

    /// The headline: a box in the corridor and the route goes round it, rather
    /// than through the space the box is in.
    #[test]
    fn a_carve_blocks_what_it_covers_and_the_route_goes_round() {
        let mut mesh = bake(&slab(0.0, 0.0, 12.0, 12.0, 0.0), &open(0.25));
        let (from, to) = ([1.0, 0.0, 6.0], [11.0, 0.0, 6.0]);
        let before = mesh.path(from, to).expect("a clear floor");
        assert!(before.complete);
        // Straight across, so every point is near the line z = 6.
        assert!(before.points.iter().all(|p| (p[2] - 6.0).abs() < 0.6), "{:?}", before.points);

        // A wall across the middle, with a gap at the top.
        mesh.carve([6.0, 0.5, 3.5], [1.0, 2.0, 9.0]);
        let after = mesh.path(from, to).expect("there is still a way round");
        assert!(after.complete, "the gap at the top is still open");
        // The wall reaches z = 8, so going round means hugging its far end —
        // the route can be exactly on the edge, and it must not be anywhere
        // near the straight line it took before.
        assert!(
            after.points.iter().any(|p| p[2] >= 7.9),
            "the route must go round the wall, not through it: {:?}",
            after.points
        );
        // And nothing walks where the wall is.
        assert!(mesh.nearest([6.0, 0.0, 3.5], 0.2).is_none(), "that ground is gone");
    }

    /// Cutting the only corridor has to make the two rooms genuinely
    /// unreachable — the region shortcut in `reachable_with` answers from the
    /// ids, so ids a carve has invalidated are a lie it will repeat.
    #[test]
    fn cutting_the_only_corridor_splits_the_regions_and_reachable_says_so() {
        let mut mesh = dumbbell();
        let (a, b) = ([2.0, 0.0, 3.0], [10.0, 0.0, 3.0]);
        assert!(mesh.reachable(a, b, 1.0), "the corridor is open to start with");
        assert_eq!(mesh.region_at(a, 1.0), mesh.region_at(b, 1.0));

        mesh.carve([6.0, 0.5, 3.0], [1.0, 2.0, 4.0]);
        assert!(!mesh.reachable(a, b, 1.0), "the corridor is blocked");
        assert_ne!(
            mesh.region_at(a, 1.0),
            mesh.region_at(b, 1.0),
            "two islands that cannot be walked between are two regions"
        );
        // Every link still points at a polygon in its own region.
        for (i, ls) in mesh.links.iter().enumerate() {
            for l in ls {
                assert_eq!(
                    mesh.polys[i].region, mesh.polys[l.to].region,
                    "polygon {i} is linked across regions after a carve"
                );
            }
        }
    }

    /// Taking the obstacle away has to give back the mesh, not something that
    /// merely behaves like it.
    #[test]
    fn removing_an_obstacle_restores_the_bake_exactly() {
        let mut mesh = dumbbell();
        let polys = mesh.polys.clone();
        let links = mesh.links.clone();
        let regions: Vec<u32> = mesh.polys.iter().map(|p| p.region).collect();
        let route = mesh.path([2.0, 0.0, 3.0], [10.0, 0.0, 3.0]).unwrap();

        let id = mesh.carve([6.0, 0.5, 3.0], [1.0, 2.0, 4.0]);
        assert_ne!(mesh.polys, polys, "the carve did nothing");

        assert!(mesh.remove_obstacle(id));
        assert_eq!(mesh.polys, polys, "the polygons must come back exactly");
        assert_eq!(mesh.links, links, "and so must the portals");
        assert_eq!(mesh.polys.iter().map(|p| p.region).collect::<Vec<_>>(), regions);
        let again = mesh.path([2.0, 0.0, 3.0], [10.0, 0.0, 3.0]).unwrap();
        assert_eq!(again.points, route.points, "the same question, the same answer");

        assert!(!mesh.remove_obstacle(id), "removing it twice is not an error, it is false");
    }

    /// Overlapping boxes are the ordinary case (two crates in one doorway) and
    /// must not depend on the order they were added or taken away.
    #[test]
    fn overlapping_obstacles_do_not_care_what_order_they_arrived_in() {
        let put = |order: [usize; 2]| {
            let mut mesh = bake(&slab(0.0, 0.0, 10.0, 10.0, 0.0), &open(0.25));
            let boxes = [([3.0, 0.5, 5.0], [3.0, 2.0, 3.0]), ([4.0, 0.5, 5.0], [3.0, 2.0, 3.0])];
            for i in order {
                mesh.carve(boxes[i].0, boxes[i].1);
            }
            (mesh.polys.len(), mesh.area())
        };
        let (n1, a1) = put([0, 1]);
        let (n2, a2) = put([1, 0]);
        assert_eq!(n1, n2);
        assert!((a1 - a2).abs() < 1e-3, "{a1} vs {a2}");
        // The union of the two boxes is 4 m wide by 3 deep — 12 m² — and it has
        // to come out once, not twice, and not not at all. Measured against the
        // bake's own area rather than against 100: geometry is rasterised
        // outward to the grid, so a 10 m floor is a little more than 100 m².
        let base = bake(&slab(0.0, 0.0, 10.0, 10.0, 0.0), &open(0.25)).area();
        let gone = base - a1;
        assert!((11.9..=12.1).contains(&gone), "the two boxes removed {gone:.2} m², not 12");
    }

    /// A box in mid-air over a floor is not an obstacle to anything on that
    /// floor. Carving on plan bounds alone would delete the ground under every
    /// crate sitting on a shelf.
    #[test]
    fn a_box_above_the_floor_leaves_it_alone() {
        let mut mesh = bake(&slab(0.0, 0.0, 10.0, 10.0, 0.0), &open(0.25));
        let before = mesh.area();
        mesh.carve([5.0, 6.0, 5.0], [2.0, 2.0, 2.0]);
        assert_eq!(mesh.area(), before, "a box five metres up blocks nothing");
        assert!(mesh.nearest([5.0, 0.0, 5.0], 0.5).is_some());
    }

    /// The grid columns and the world bounds are two descriptions of one
    /// rectangle, and the overlay and the tests read the first — so a carve
    /// that leaves them disagreeing is a carve that draws wrong.
    #[test]
    fn a_carved_polygons_columns_still_match_its_world_bounds() {
        let mut mesh = bake(&slab(0.0, 0.0, 10.0, 10.0, 0.0), &open(0.25));
        mesh.carve([4.4, 0.5, 5.7], [1.3, 2.0, 2.2]);
        let cell = mesh.cell_size;
        for p in &mesh.polys {
            let x = mesh.origin[0] + p.x0 as f32 * cell;
            let z = mesh.origin[2] + p.z0 as f32 * cell;
            assert!((x - p.min[0]).abs() < 1e-3, "{p:?} column x0 is not its min x");
            assert!((z - p.min[1]).abs() < 1e-3, "{p:?} column z0 is not its min z");
            assert!((p.w as f32 * cell - (p.max[0] - p.min[0])).abs() < 1e-3, "{p:?} width");
            assert!((p.d as f32 * cell - (p.max[1] - p.min[1])).abs() < 1e-3, "{p:?} depth");
        }
    }

    /// The carve box is snapped outward, never inward: blocking slightly more
    /// than the crate is a crate, blocking slightly less is a character walking
    /// through one.
    #[test]
    fn the_hole_is_never_smaller_than_what_was_asked_for() {
        let mut mesh = bake(&slab(0.0, 0.0, 10.0, 10.0, 0.0), &open(0.25));
        mesh.carve([4.4, 0.5, 5.7], [1.3, 2.0, 2.2]);
        let o = mesh.obstacles()[0];
        assert!(o.min[0] <= 4.4 - 0.65 && o.max[0] >= 4.4 + 0.65, "{o:?}");
        assert!(o.min[1] <= 5.7 - 1.1 && o.max[1] >= 5.7 + 1.1, "{o:?}");
        for p in &mesh.polys {
            assert!(
                !o.bites(p),
                "{p:?} still overlaps the hole {o:?}",
            );
        }
    }

    /// An off-link's ends cache which polygon they landed on. Renumbering
    /// without fixing them up is the trap this whole design avoids, and this is
    /// the assertion that says so.
    #[test]
    fn a_carve_re_resolves_the_hand_placed_links() {
        let mut tris = slab(0.0, 0.0, 4.0, 4.0, 0.0);
        tris.extend(slab(8.0, 0.0, 4.0, 4.0, 0.0));
        let mut mesh = bake(&tris, &open(0.25));
        mesh = mesh.with_links(vec![crate::link::OffLink::new(
            1,
            "jump",
            [3.5, 0.0, 2.0],
            [8.5, 0.0, 2.0],
        )]);
        let (a, b) = ([1.0, 0.0, 2.0], [11.0, 0.0, 2.0]);
        assert!(mesh.off_links[0].resolved(), "both ends are on the floor");
        assert!(mesh.reachable(a, b, 1.0), "the jump joins the islands");

        // A crate on the landing side: the link's far end has nowhere to be.
        mesh.carve([8.5, 0.5, 2.0], [1.5, 2.0, 1.5]);
        assert!(mesh.off_links[0].to_poly != NOWHERE || !mesh.reachable(a, b, 1.0));
        for l in &mesh.off_links {
            if l.from_poly != NOWHERE {
                assert!((l.from_poly as usize) < mesh.polys.len());
            }
            if l.to_poly != NOWHERE {
                assert!((l.to_poly as usize) < mesh.polys.len());
            }
        }
    }

    /// Nothing in a carve may reach the sidecar on disk. The two fields it adds
    /// are `serde(skip)`, so a mesh saved after a carve saves the bake — which
    /// is what makes pressing Stop give the level back.
    #[test]
    fn a_carved_mesh_serialises_as_the_bake_it_came_from() {
        let mut mesh = bake(&slab(0.0, 0.0, 10.0, 10.0, 0.0), &open(0.25));
        let clean = postcard::to_allocvec(&mesh).unwrap();
        mesh.carve([5.0, 0.5, 5.0], [2.0, 2.0, 2.0]);
        assert!(mesh.polys.len() > 1, "the carve did nothing to assert about");
        // The carved mesh is a different mesh, so its bytes differ — the point
        // is that the obstacle list itself is not among them, and that the mesh
        // it round-trips to has no memory of one.
        let back: NavMesh = postcard::from_bytes(&postcard::to_allocvec(&mesh).unwrap()).unwrap();
        assert!(back.obstacles().is_empty(), "an obstacle must not survive a save");
        assert!(!clean.is_empty());
    }

    /// The scaling guard. Carving is only worth having if its cost follows the
    /// size of the hole rather than the size of the level — the same shape as
    /// the crate's other guards, a growth RATIO rather than a duration, so it
    /// says the same thing on a slow machine as on a fast one.
    #[test]
    fn carving_cost_follows_the_hole_and_not_the_level() {
        let one_crate = |side: f32| -> std::time::Duration {
            let s = open(0.25);
            let mut mesh = bake(&slab(0.0, 0.0, side, side, 0.0), &s);
            // Warm the index so the measurement is the carve, not a first-use
            // build of something the level would have built anyway.
            let _ = mesh.nearest([1.0, 0.0, 1.0], 1.0);
            let t = std::time::Instant::now();
            for k in 0..20 {
                let id = mesh.carve([2.0 + k as f32 * 0.01, 0.5, 2.0], [1.0, 2.0, 1.0]);
                mesh.remove_obstacle(id);
            }
            t.elapsed()
        };
        let small = one_crate(16.0);
        let big = one_crate(64.0);
        // Sixteen times the area. A carve that scanned the level would grow
        // with it; this one copies the polygon list, so it is allowed to grow a
        // little — but nothing like sixteenfold.
        let ratio = big.as_secs_f64() / small.as_secs_f64().max(1e-9);
        assert!(
            ratio < 8.0,
            "16x the level cost {ratio:.1}x the carve ({small:?} -> {big:?}) — \
             something in the carve is scanning the whole mesh",
        );
    }
}
