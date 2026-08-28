//! Replacing one box of a navmesh with a freshly baked one.
//!
//! A level that builds itself while somebody walks through it — a streamer, a
//! generated dungeon, a building coming down — changes a *box* at a time and
//! then has to re-measure. Re-measuring the whole level to account for one
//! 32 m chunk is the wrong unit of work by construction: the amount of new level
//! per crossing is constant, so the cost per crossing should be too, and instead
//! it grows with how much of the level is loaded.
//!
//! # Why this is not carving
//!
//! [`NavMesh::carve`](crate::NavMesh::carve) takes ground *away* and can put it
//! back, because the box it cut is a thing standing on a level that has not
//! changed. A splice is the opposite claim: **the level here is different now**,
//! and the new measurement is the truth. So a splice writes through to the bake
//! rather than sitting on top of it, and any obstacles carved into the old mesh
//! are re-applied to the new one afterwards.
//!
//! # The seam is the whole problem
//!
//! Two navmeshes do not join just because they are adjacent. A portal exists
//! between two rectangles that **share an edge**, so both sides have to end on
//! exactly the same line — and the surviving polygons of the host mesh end
//! wherever the old bake happened to put them, which is not the box's edge.
//!
//! So the host's polygons are **cut** at the box, by the same `subtract` a carve
//! uses, and the incoming ones are clipped to it. Both sides then end on the
//! same coordinate to the last bit, and the bake's own portal rule — stated
//! geometrically in [`super::carve`] — finds the joins with nothing special
//! about the seam at all.
//!
//! This is also why the grids do **not** have to line up. Only the two edges at
//! the seam have to agree, and clipping both sides to one number is what makes
//! them.

use crate::carve::{child, portal, subtract, Obstacle};
use crate::link::NOWHERE;
use crate::mesh::oriented;
use crate::{Link, NavMesh, Poly};

/// Why a splice could not be done.
///
/// Every one of these is a caller mistake rather than a state the engine can
/// get into on its own, and each names the number that has to change — a splice
/// that silently did the wrong thing would produce a navmesh that looks right
/// and routes wrong, which is the failure this whole area keeps producing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SpliceError {
    /// The two meshes are measured around different points, so their polygons
    /// are not in the same space and nothing about them can be compared.
    DifferentAnchor,
    /// The incoming mesh was baked for a different character. Splicing it would
    /// leave one level with two erosion radii in it.
    DifferentSettings,
    /// The box has no width in plan.
    EmptyBox,
}

impl std::fmt::Display for SpliceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpliceError::DifferentAnchor => write!(
                f,
                "the region was baked around a different anchor — bake it with the mesh's own \
                 origin and anchor, or its polygons describe somewhere else"
            ),
            SpliceError::DifferentSettings => write!(
                f,
                "the region was baked for a different character — one level cannot have two \
                 agent radii in it"
            ),
            SpliceError::EmptyBox => write!(f, "the region has no width"),
        }
    }
}

impl std::error::Error for SpliceError {}

impl NavMesh {
    /// Replace everything inside a box with a freshly baked `region`.
    ///
    /// `centre` and `size` are in this mesh's own frame (see
    /// [`NavMesh::to_local`]), and the box is snapped **outward** to this bake's
    /// grid — so read the mesh back rather than assuming the box you asked for
    /// is the box that changed.
    ///
    /// `region` must have been baked with this mesh's settings and anchor. It
    /// does not have to be baked on the same grid, and it does not have to be
    /// clipped to the box: anything of it outside the box is dropped here.
    ///
    /// Returns how many polygons the mesh has afterwards.
    ///
    /// The caller still has to tell the crowd. Nothing here can reach it, and
    /// [`Crowd::navmesh_changed`](crate::Crowd::navmesh_changed) is the call
    /// that makes an agent mid-route notice the ground moved.
    pub fn splice(
        &mut self,
        centre: [f32; 3],
        size: [f32; 3],
        region: &NavMesh,
    ) -> Result<usize, SpliceError> {
        if region.anchor != self.anchor {
            return Err(SpliceError::DifferentAnchor);
        }
        if region.settings != self.settings {
            return Err(SpliceError::DifferentSettings);
        }
        let cell = self.cell_size.max(1e-4);
        let half = [size[0].abs() * 0.5, size[1].abs() * 0.5, size[2].abs() * 0.5];
        if half[0] <= 0.0 || half[2] <= 0.0 {
            return Err(SpliceError::EmptyBox);
        }
        // Outward to whole cells, from this bake's origin — the same snapping a
        // carve does, for the same reason: the box has to land on the grid the
        // polygons were cut from.
        let snap_lo = |v: f32, o: f32| o + ((v - o) / cell).floor() * cell;
        let snap_hi = |v: f32, o: f32| o + ((v - o) / cell).ceil() * cell;
        let box_ = Obstacle {
            id: 0,
            min: [
                snap_lo(centre[0] - half[0], self.origin[0]),
                snap_lo(centre[2] - half[2], self.origin[2]),
            ],
            max: [
                snap_hi(centre[0] + half[0], self.origin[0]),
                snap_hi(centre[2] + half[2], self.origin[2]),
            ],
            y_min: centre[1] - half[1],
            y_max: centre[1] + half[1],
        };

        // Splice into the BAKE, not into whatever the obstacles have made of it:
        // a rebake is a new measurement of the level and becomes the thing
        // carves are re-derived from. `recarve` at the end puts them back.
        let base = self.baked.take().map(|b| *b).unwrap_or_else(|| crate::carve::Baked {
            polys: self.polys.clone(),
            links: self.links.clone(),
            off_links: self.off_links.clone(),
        });

        let hit: Vec<bool> = base.polys.iter().map(|p| box_.bites(p)).collect();
        let one = [box_];

        // ---- 1. the surviving polygons, cut at the box ----------------------
        let mut map = vec![NOWHERE as usize; base.polys.len()];
        let mut polys: Vec<Poly> = Vec::with_capacity(base.polys.len() + region.polys.len());
        // Which polygons ended up on the seam side of the cut, so the relink
        // below can be local rather than a rebuild of the whole level.
        let mut cut: Vec<usize> = Vec::new();
        for (i, p) in base.polys.iter().enumerate() {
            if !hit[i] {
                map[i] = polys.len();
                polys.push(*p);
                continue;
            }
            for r in subtract(p, &one) {
                cut.push(polys.len());
                polys.push(child(p, r, self.origin, cell));
            }
        }

        // ---- 2. the incoming polygons, clipped to the box -------------------
        //
        // The bake of a region spills past its box — erosion and rasterisation
        // both work in whole cells — and a polygon reaching out over ground the
        // host still owns would be two floors in one place.
        let fresh_from = polys.len();
        let mut fresh_map = vec![NOWHERE as usize; region.polys.len()];
        // Region ids have to be unique before anything is joined up; the flood
        // below decides what they really are.
        let base_region = base.polys.iter().map(|p| p.region).max().unwrap_or(0) + 1;
        for (i, p) in region.polys.iter().enumerate() {
            let min = [p.min[0].max(box_.min[0]), p.min[1].max(box_.min[1])];
            let max = [p.max[0].min(box_.max[0]), p.max[1].min(box_.max[1])];
            if max[0] - min[0] <= 1e-4 || max[1] - min[1] <= 1e-4 {
                continue;
            }
            fresh_map[i] = polys.len();
            let mut q = child(p, (min, max), self.origin, cell);
            q.region = base_region + p.region;
            polys.push(q);
        }

        // ---- 3. carry over every link whose both ends survived --------------
        let mut links: Vec<Vec<Link>> = vec![Vec::new(); polys.len()];
        for (i, ls) in base.links.iter().enumerate() {
            if hit[i] {
                continue; // the cut pieces are relinked below
            }
            for l in ls {
                if !hit[l.to] {
                    links[map[i]].push(Link { to: map[l.to], ..*l });
                }
            }
        }
        // …and the incoming mesh's own portals, which its bake already derived.
        for (i, ls) in region.links.iter().enumerate() {
            if fresh_map[i] == NOWHERE as usize {
                continue;
            }
            for l in ls {
                if fresh_map[l.to] != NOWHERE as usize {
                    links[fresh_map[i]].push(Link { to: fresh_map[l.to], ..*l });
                }
            }
        }

        // ---- 4. relink across the seam, and only there ----------------------
        //
        // A portal can only have appeared or disappeared where the cut happened.
        // Everything else kept its links verbatim above, which is both cheaper
        // and safer: re-measuring one chunk must not change a route on the far
        // side of the level.
        let step = self.settings.step_height.max(0.0);
        let mut seam: Vec<usize> = cut.clone();
        seam.extend(fresh_from..polys.len());
        for (i, ls) in base.links.iter().enumerate() {
            if !hit[i] {
                continue;
            }
            for l in ls {
                if !hit[l.to] {
                    seam.push(map[l.to]); // a survivor that used to border the box
                }
            }
        }
        seam.sort_unstable();
        seam.dedup();
        let fresh_range = fresh_from..polys.len();
        for (ai, &a) in seam.iter().enumerate() {
            for &b in &seam[ai + 1..] {
                // Two polygons that both came through untouched already have
                // whatever portal the bake gave them; re-deriving it here could
                // only disagree with it.
                let touched = cut.binary_search(&a).is_ok()
                    || cut.binary_search(&b).is_ok()
                    || fresh_range.contains(&a)
                    || fresh_range.contains(&b);
                if !touched {
                    continue;
                }
                // …and two polygons INSIDE the incoming mesh were linked by its
                // own bake in step 3.
                if fresh_range.contains(&a) && fresh_range.contains(&b) {
                    continue;
                }
                let Some((p, q)) = portal(&polys[a], &polys[b], step) else { continue };
                links[a].push(oriented(polys[a].centre, polys[b].centre, b, p, q));
                links[b].push(oriented(polys[b].centre, polys[a].centre, a, p, q));
            }
        }
        // The bake's own comparator: a route that comes out in a different order
        // between runs is a bug nobody can reproduce.
        for l in &mut links {
            l.sort_by(|a, b| {
                a.to.cmp(&b.to)
                    .then(a.left[0].total_cmp(&b.left[0]))
                    .then(a.left[2].total_cmp(&b.left[2]))
            });
            l.dedup_by(|a, b| a.to == b.to && a.left == b.left && a.right == b.right);
        }

        // ---- 5. the new bake ------------------------------------------------
        self.polys = polys;
        self.links = links;
        self.off_links = base.off_links;
        self.renumber_regions();
        self.baked = Some(Box::new(crate::carve::Baked {
            polys: self.polys.clone(),
            links: self.links.clone(),
            off_links: self.off_links.clone(),
        }));
        self.index.take();
        self.link_index.take();
        self.island_index.take();
        self.summary_cache.take();

        // Carves are things standing on the level, and re-measuring the ground
        // under a crate does not move the crate. Any that no longer bite
        // anything simply take nothing out.
        self.recarve();
        Ok(self.polys.len())
    }

    /// An empty mesh in this one's frame, for splicing a box that now has
    /// nothing walkable in it.
    ///
    /// A demolition is a rebake like any other — the answer just happens to be
    /// no floor — and it needs a `region` to hand [`NavMesh::splice`]. Baking
    /// nothing produces nothing, so this is the nothing.
    pub fn empty_like(&self) -> NavMesh {
        NavMesh {
            polys: Vec::new(),
            links: Vec::new(),
            origin: self.origin,
            cell_size: self.cell_size,
            anchor: self.anchor,
            settings: self.settings,
            areas: self.areas.clone(),
            off_links: Vec::new(),
            index: std::sync::OnceLock::new(),
            link_index: std::sync::OnceLock::new(),
            island_index: std::sync::OnceLock::new(),
            summary_cache: std::sync::OnceLock::new(),
            obstacles: Vec::new(),
            next_obstacle: 0,
            baked: None,
            obstacle_rev: 0,
        }
    }

    /// Give every island a region id, keeping the ones that still describe the
    /// same island.
    ///
    /// A splice can **merge** islands (the new chunk joins two wings that were
    /// separate) as easily as split them, and `resplit_regions` only splits. So
    /// this is the honest version: flood the whole mesh, and let each island
    /// claim the lowest id it already contained.
    ///
    /// It is O(polygons + links) and a splice is not a per-frame operation. The
    /// property worth paying for is that an island nothing happened to keeps its
    /// number — the editor's overlay does not recolour the level, and an id a
    /// script wrote down still means what it meant.
    ///
    /// Two islands cannot both keep the same old id: the one holding the lowest
    /// polygon index keeps it and the rest are minted above the highest, in
    /// index order, so the numbering is a function of the mesh rather than of
    /// the order anything arrived in.
    fn renumber_regions(&mut self) {
        let n = self.polys.len();
        let mut island = vec![usize::MAX; n];
        let mut islands: Vec<Vec<usize>> = Vec::new();
        for seed in 0..n {
            if island[seed] != usize::MAX {
                continue;
            }
            let id = islands.len();
            let mut members = vec![seed];
            let mut stack = vec![seed];
            island[seed] = id;
            while let Some(i) = stack.pop() {
                for l in &self.links[i] {
                    if island[l.to] == usize::MAX {
                        island[l.to] = id;
                        members.push(l.to);
                        stack.push(l.to);
                    }
                }
            }
            islands.push(members);
        }

        let mut taken: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let mut wanted: Vec<Option<u32>> = Vec::with_capacity(islands.len());
        for members in &islands {
            // The lowest id in the island, and it must not already be claimed:
            // the island holding the lowest polygon index gets first refusal,
            // and `islands` is built in polygon order, so iterating it in order
            // is that rule.
            let want = members.iter().map(|&i| self.polys[i].region).min();
            wanted.push(match want {
                Some(r) if taken.insert(r) => Some(r),
                _ => None,
            });
        }
        let mut next = self.polys.iter().map(|p| p.region).max().unwrap_or(0) + 1;
        for (members, want) in islands.iter().zip(&wanted) {
            let id = match want {
                Some(r) => *r,
                None => {
                    while !taken.insert(next) {
                        next += 1;
                    }
                    next
                }
            };
            for &i in members {
                self.polys[i].region = id;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{bake, NavSettings, Tri};

    fn quad(x0: f32, x1: f32, z0: f32, z1: f32, y: f32) -> Vec<Tri> {
        vec![
            Tri::new([x0, y, z0], [x1, y, z0], [x0, y, z1]),
            Tri::new([x1, y, z0], [x1, y, z1], [x0, y, z1]),
        ]
    }

    fn settings() -> NavSettings {
        NavSettings { agent_radius: 0.0, cell_size: 0.5, ..Default::default() }
    }

    /// A 24x8 corridor, and the middle third of it spliced back in unchanged.
    ///
    /// **This is the test that matters.** If the seam does not join, the level
    /// falls into three islands and a route from one end to the other stops
    /// existing — and it stops existing *quietly*, which is the whole failure
    /// mode a splice can have.
    #[test]
    fn re_measuring_a_box_with_the_same_ground_changes_nothing_you_can_walk() {
        let floor = quad(0.0, 24.0, 0.0, 8.0, 0.0);
        let mut mesh = bake(&floor, &settings()).expect("the corridor bakes");
        let before = mesh.area();
        let from = [1.0, 0.0, 4.0];
        let to = [23.0, 0.0, 4.0];
        assert!(mesh.path(from, to).expect("a route before").complete);

        // The same ground, measured again, over the middle third.
        let region = bake(&quad(8.0, 16.0, 0.0, 8.0, 0.0), &settings()).expect("the region bakes");
        let n = mesh.splice([12.0, 0.0, 4.0], [8.0, 4.0, 8.0], &region).expect("it splices");
        assert!(n > 0);

        let path = mesh.path(from, to).expect("a route after");
        assert!(path.complete, "the seam did not join — the level is in pieces");
        assert!(
            (mesh.area() - before).abs() < 0.6,
            "the walkable area moved: {before} -> {}",
            mesh.area()
        );
        // One island, still.
        let r = mesh.region_at(from, 1.0).expect("an island");
        assert_eq!(mesh.region_at(to, 1.0), Some(r), "the two ends are on different islands");
    }

    /// Ground that went away goes away, and the route round it is found.
    #[test]
    fn re_measuring_a_box_that_lost_its_floor_cuts_the_route() {
        // A ring: two long sides joined at both ends, so there are two ways
        // round and removing one leaves the other.
        let mut floor = quad(0.0, 24.0, 0.0, 4.0, 0.0);
        floor.extend(quad(0.0, 24.0, 12.0, 16.0, 0.0));
        floor.extend(quad(0.0, 4.0, 4.0, 12.0, 0.0));
        floor.extend(quad(20.0, 24.0, 4.0, 12.0, 0.0));
        let mut mesh = bake(&floor, &settings()).expect("the ring bakes");
        // Both ends on the NEAR side, so the demolished middle is on the only
        // straight route between them.
        let from = [2.0, 0.0, 2.0];
        let to = [22.0, 0.0, 2.0];
        assert!(mesh.path(from, to).expect("a route before").complete);

        // The near side of the ring is demolished: a region with nothing in it.
        let empty = bake(&quad(100.0, 108.0, 100.0, 108.0, 0.0), &settings()).expect("bakes");
        // Tall enough in z to take the whole near side with it. A bake
        // rasterises OUTWARD, so a box drawn exactly on the floor's nominal
        // edge leaves a half-cell strip of ground behind — and a half-cell
        // strip is a corridor.
        mesh.splice([12.0, 0.0, 2.0], [8.0, 4.0, 8.0], &empty).expect("it splices");

        let path = mesh.path(from, to).expect("a route after");
        assert!(path.complete, "the long way round should still exist");
        // …and it really did go the long way.
        assert!(
            path.points.iter().any(|p| p[2] > 8.0),
            "the route did not go round the far side: {:?}",
            path.points
        );
    }

    /// A splice becomes the bake, so a carve outstanding at the time survives it
    /// — and removing that carve gives back the NEW ground, not the old.
    #[test]
    fn a_carve_survives_a_splice_and_still_lets_go() {
        let mut mesh = bake(&quad(0.0, 24.0, 0.0, 8.0, 0.0), &settings()).expect("bakes");
        // The bake's own figure, not a nominal 24x8: rasterisation rounds
        // outward, so the floor measures a little over what it was drawn as.
        let whole = mesh.area();
        let id = mesh.carve([4.0, 0.0, 4.0], [2.0, 2.0, 2.0]);
        let carved = mesh.area();
        assert!(carved < whole, "the crate blocked nothing to begin with");

        let region = bake(&quad(8.0, 16.0, 0.0, 8.0, 0.0), &settings()).expect("bakes");
        mesh.splice([12.0, 0.0, 4.0], [8.0, 4.0, 8.0], &region).expect("it splices");
        assert!(
            (mesh.area() - carved).abs() < 0.6,
            "the crate stopped blocking when the ground beside it was re-measured"
        );

        assert!(mesh.remove_obstacle(id));
        assert!(mesh.obstacles().is_empty());
        assert!(
            (mesh.area() - whole).abs() < 0.01,
            "removing it did not give the floor back: {} against {whole}",
            mesh.area()
        );
    }

    /// Two wings with nothing between them, joined by re-measuring the gap.
    ///
    /// The case `resplit_regions` cannot do, and the reason regions are worked
    /// out by flooding rather than by splitting: a splice MERGES islands as
    /// readily as it breaks them.
    #[test]
    fn splicing_a_bridge_in_merges_the_islands_it_joins() {
        let mut floor = quad(0.0, 8.0, 0.0, 8.0, 0.0);
        floor.extend(quad(16.0, 24.0, 0.0, 8.0, 0.0));
        let mut mesh = bake(&floor, &settings()).expect("two wings bake");
        let left = [2.0, 0.0, 4.0];
        let right = [22.0, 0.0, 4.0];
        assert_ne!(mesh.region_at(left, 1.0), mesh.region_at(right, 1.0), "they start apart");
        assert!(!mesh.reachable(left, right, 1.0));

        // The middle, measured, with floor in it this time.
        let region = bake(&quad(6.0, 18.0, 2.0, 6.0, 0.0), &settings()).expect("the bridge bakes");
        mesh.splice([12.0, 0.0, 4.0], [12.0, 4.0, 4.0], &region).expect("it splices");

        assert!(mesh.reachable(left, right, 1.0), "the bridge did not join the wings");
        assert_eq!(
            mesh.region_at(left, 1.0),
            mesh.region_at(right, 1.0),
            "they are walkable between and still say they are different islands"
        );
    }

    /// The two guards, because a splice that quietly did the wrong thing is the
    /// thing to be afraid of here.
    #[test]
    fn a_region_from_a_different_bake_is_refused_rather_than_used() {
        let mut mesh = bake(&quad(0.0, 24.0, 0.0, 8.0, 0.0), &settings()).expect("bakes");
        let elsewhere = bake(&quad(0.0, 8.0, 0.0, 8.0, 0.0), &settings())
            .expect("bakes")
            .anchored_at([1000.0, 0.0, 0.0]);
        assert_eq!(
            mesh.splice([4.0, 0.0, 4.0], [4.0, 4.0, 4.0], &elsewhere),
            Err(super::SpliceError::DifferentAnchor)
        );

        let fatter = bake(
            &quad(0.0, 8.0, 0.0, 8.0, 0.0),
            &NavSettings { agent_radius: 1.0, cell_size: 0.5, ..Default::default() },
        )
        .expect("bakes");
        assert_eq!(
            mesh.splice([4.0, 0.0, 4.0], [4.0, 4.0, 4.0], &fatter),
            Err(super::SpliceError::DifferentSettings)
        );
    }

    /// A hand-placed link whose ground was re-measured finds it again.
    #[test]
    fn an_off_link_re_resolves_onto_the_new_ground() {
        let mut floor = quad(0.0, 8.0, 0.0, 8.0, 0.0);
        floor.extend(quad(16.0, 24.0, 0.0, 8.0, 0.0));
        let plank = crate::OffLink::new(1, "plank", [7.0, 0.0, 4.0], [17.0, 0.0, 4.0]);
        let mut mesh = crate::bake_with(&floor, &settings(), &[], vec![plank])
            .expect("two wings and a plank bake");
        assert!(mesh.off_links[0].resolved(), "the plank starts resolved");

        let region = bake(&quad(0.0, 8.0, 0.0, 8.0, 0.0), &settings()).expect("bakes");
        mesh.splice([4.0, 0.0, 4.0], [8.0, 4.0, 8.0], &region).expect("it splices");
        assert!(
            mesh.off_links[0].resolved(),
            "the plank lost the ground it was standing on when that ground was re-measured"
        );
    }
}
