//! From cells to polygons — the navmesh itself.
//!
//! A grid of walkable cells is already enough to search, but searching it is
//! searching every square metre of the level. Cutting each region into a few
//! large **rectangles** turns that into a search over a few dozen shapes, and
//! gives the path smoother something to work with: rectangles are convex by
//! construction and share whole edges, which is exactly what a funnel needs.
//!
//! The cut is greedy and takes one pass. Walk the cells in order; the first one
//! that has not been claimed becomes the corner of a new rectangle, which grows
//! as far right as it can and then as far forward as it can while every row
//! stays complete. It is not the smallest possible set of rectangles — that
//! problem is not worth solving here — but it is close, it is fast, and it never
//! produces a shape that is not convex.
//!
//! # What a rectangle is allowed to cover
//!
//! Cells within `step_height` of each other, total. A floor comes out as one
//! rectangle; a ramp comes out as a run of them, each a step tall. That keeps a
//! polygon's height meaningful — a single rectangle spanning a whole staircase
//! would have to claim one height for ground that is metres apart.
//!
//! # Portals
//!
//! Two rectangles that share a run of cell edges are linked, and the link
//! carries that shared run as a **portal**: the segment you must cross to get
//! from one to the other. The funnel in [`crate::path`] pulls the path taut
//! against those segments, which is what turns a staircase of cell moves into a
//! straight line across a room.
//!
//! Each portal's endpoints are stored **left and right as seen walking through
//! it**, once per direction, so the funnel never has to work out which side of
//! itself it is on.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::walkable::WalkableGrid;
use crate::NavSettings;

/// One convex piece of the walkable surface: an axis-aligned rectangle in plan,
/// with the height range of the ground inside it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Poly {
    /// Column coordinates of the corner, and the size in columns. Kept for
    /// debug drawing and tests; everything else works in world units.
    pub x0: usize,
    pub z0: usize,
    pub w: usize,
    pub d: usize,
    /// Which walkable region this belongs to. Polygons in different regions can
    /// never be linked, so this is also "which island of the level is it on".
    pub region: u32,
    /// World-space plan bounds, as (x, z).
    pub min: [f32; 2],
    pub max: [f32; 2],
    pub y_min: f32,
    pub y_max: f32,
    /// The middle of the rectangle, at the middle of its height range. What A*
    /// measures distances between.
    pub centre: [f32; 3],
}

impl Poly {
    /// Whether a world point is over this rectangle in plan.
    pub fn contains_xz(&self, p: [f32; 3]) -> bool {
        p[0] >= self.min[0] && p[0] <= self.max[0] && p[2] >= self.min[1] && p[2] <= self.max[1]
    }

    /// The nearest point on this polygon to `p` — clamped into the rectangle in
    /// plan and into the height range vertically.
    pub fn clamp(&self, p: [f32; 3]) -> [f32; 3] {
        [
            p[0].clamp(self.min[0], self.max[0]),
            p[1].clamp(self.y_min, self.y_max),
            p[2].clamp(self.min[1], self.max[1]),
        ]
    }
}

/// A way out of one polygon and into another.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub to: usize,
    /// The portal's endpoints, as seen by something walking from the polygon
    /// that owns this link into `to`.
    pub left: [f32; 3],
    pub right: [f32; 3],
}

impl Link {
    pub fn midpoint(&self) -> [f32; 3] {
        [
            (self.left[0] + self.right[0]) * 0.5,
            (self.left[1] + self.right[1]) * 0.5,
            (self.left[2] + self.right[2]) * 0.5,
        ]
    }
}

/// The baked navmesh: convex polygons and the portals between them.
///
/// Serializable, because a bake is a build artefact that belongs beside the
/// scene rather than something to redo on every load.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NavMesh {
    pub polys: Vec<Poly>,
    /// `links[i]` is everywhere you can go from `polys[i]`.
    pub links: Vec<Vec<Link>>,
    pub origin: [f32; 3],
    pub cell_size: f32,
    /// What it was baked with. Kept so a mesh can answer questions about the
    /// character it was baked for — how far off it a point may be and still
    /// count as on it, how wide to draw it — without every caller having to
    /// carry the settings alongside.
    pub settings: NavSettings,
}

impl NavMesh {
    /// Cut a walkable grid into polygons and link them.
    ///
    /// `None` when the grid holds nothing — which [`WalkableGrid::build`]
    /// already refuses to produce, so this is belt and braces rather than a case
    /// worth handling twice.
    pub fn build(grid: &WalkableGrid, settings: &NavSettings) -> Option<NavMesh> {
        if grid.cells.is_empty() {
            return None;
        }
        let cell = grid.cell_size;
        let span = settings.step_height.max(0.0);

        let mut owner = vec![usize::MAX; grid.cells.len()];
        let mut polys: Vec<Poly> = Vec::new();

        // Cells come out of the grid in column order, so seeding in order means
        // rectangles are grown from their own top-left corner and the greedy
        // choice is the natural one.
        for seed in 0..grid.cells.len() {
            if owner[seed] != usize::MAX {
                continue;
            }
            let start = grid.cells[seed];
            let region = grid.region[seed];
            let (mut y_min, mut y_max) = (start.y, start.y);

            // Grow right along the seed's row.
            let mut rows: Vec<Vec<usize>> = vec![vec![seed]];
            let mut x = start.x + 1;
            while x < grid.width {
                let Some(i) = pick(x, start.z, region, y_min, y_max, span, grid, &owner)
                else {
                    break;
                };
                let y = grid.cells[i].y;
                y_min = y_min.min(y);
                y_max = y_max.max(y);
                rows[0].push(i);
                x += 1;
            }
            let w = rows[0].len();

            // Then grow forward, one whole row at a time. A row that cannot be
            // completed ends the rectangle: a ragged edge would not be convex.
            let mut z = start.z + 1;
            while z < grid.depth {
                let mut row = Vec::with_capacity(w);
                let (mut ry_min, mut ry_max) = (y_min, y_max);
                for k in 0..w {
                    let Some(i) =
                        pick(start.x + k, z, region, ry_min, ry_max, span, grid, &owner)
                    else {
                        break;
                    };
                    let y = grid.cells[i].y;
                    ry_min = ry_min.min(y);
                    ry_max = ry_max.max(y);
                    row.push(i);
                }
                if row.len() < w {
                    break;
                }
                y_min = ry_min;
                y_max = ry_max;
                rows.push(row);
                z += 1;
            }

            let id = polys.len();
            for row in &rows {
                for &i in row {
                    owner[i] = id;
                }
            }
            let d = rows.len();
            let min = [
                grid.origin[0] + start.x as f32 * cell,
                grid.origin[2] + start.z as f32 * cell,
            ];
            let max = [min[0] + w as f32 * cell, min[1] + d as f32 * cell];
            polys.push(Poly {
                x0: start.x,
                z0: start.z,
                w,
                d,
                region,
                min,
                max,
                y_min,
                y_max,
                centre: [
                    (min[0] + max[0]) * 0.5,
                    (y_min + y_max) * 0.5,
                    (min[1] + max[1]) * 0.5,
                ],
            });
        }

        let links = link_polys(grid, &owner, &polys, settings);
        Some(NavMesh {
            polys,
            links,
            origin: grid.origin,
            cell_size: cell,
            settings: *settings,
        })
    }

    /// The polygon a point is on, or the nearest one within `max_distance`.
    ///
    /// Returns the polygon and the point snapped onto it, because a caller
    /// asking "where am I on the navmesh" almost always needs both — and a
    /// character standing a few centimetres above the floor, or just off the
    /// edge of it, is the normal case rather than an error.
    pub fn nearest(&self, p: [f32; 3], max_distance: f32) -> Option<(usize, [f32; 3])> {
        let mut best: Option<(usize, [f32; 3], f32)> = None;
        for (i, poly) in self.polys.iter().enumerate() {
            let q = poly.clamp(p);
            let d = dist(p, q);
            if d > max_distance {
                continue;
            }
            // A tie in distance goes to the polygon the point is actually over:
            // standing on a bridge must not snap to the ground beneath it.
            let better = match best {
                None => true,
                Some((_, _, bd)) => d < bd - 1e-6,
            };
            if better {
                best = Some((i, q, d));
            }
        }
        best.map(|(i, q, _)| (i, q))
    }

    /// Total walkable area, in square metres. The one number worth printing
    /// after a bake: it is the size of the level a character can reach, and it
    /// moving when you did not expect it to move is the whole point of having
    /// it.
    pub fn area(&self) -> f32 {
        self.polys
            .iter()
            .map(|p| (p.max[0] - p.min[0]) * (p.max[1] - p.min[1]))
            .sum()
    }
}

/// The best cell in one column to add to a rectangle, or `None` if there is not
/// one that fits.
///
/// "Fits" is: unclaimed, in the same region, and inside the rectangle's height
/// span once it is added. Where a column offers two (a bridge over a floor) the
/// one nearest the rectangle's own height wins.
#[allow(clippy::too_many_arguments)]
fn pick(
    x: usize,
    z: usize,
    region: u32,
    y_min: f32,
    y_max: f32,
    span: f32,
    grid: &WalkableGrid,
    owner: &[usize],
) -> Option<usize> {
    let mid = (y_min + y_max) * 0.5;
    let mut best: Option<(usize, f32)> = None;
    for i in grid.column_range(x, z) {
        if owner[i] != usize::MAX || grid.region[i] != region {
            continue;
        }
        let y = grid.cells[i].y;
        if y_max.max(y) - y_min.min(y) > span {
            continue;
        }
        let d = (y - mid).abs();
        if best.is_none_or(|(_, bd)| d < bd) {
            best = Some((i, d));
        }
    }
    best.map(|(i, _)| i)
}

/// Where two rectangles touch, keyed so that two rectangles which touch along
/// two separate boundaries — possible when one passes over the other — get two
/// portals rather than one nonsense one spanning both.
type PortalKey = (usize, usize, u8, usize);

/// Accumulated extent of one portal, in cells, plus the height either side.
struct PortalSpan {
    lo: usize,
    hi: usize,
    y_sum: f32,
    n: u32,
}

fn link_polys(
    grid: &WalkableGrid,
    owner: &[usize],
    polys: &[Poly],
    settings: &NavSettings,
) -> Vec<Vec<Link>> {
    let step = settings.step_height;
    let mut spans: HashMap<PortalKey, PortalSpan> = HashMap::new();

    for (i, c) in grid.cells.iter().enumerate() {
        // Only +x and +z, so each shared edge is visited once.
        for (dx, dz) in [(1usize, 0usize), (0, 1)] {
            let (nx, nz) = (c.x + dx, c.z + dz);
            if nx >= grid.width || nz >= grid.depth {
                continue;
            }
            for j in grid.column_range(nx, nz) {
                let n = grid.cells[j];
                if (n.y - c.y).abs() > step || owner[i] == owner[j] {
                    continue;
                }
                let (a, b) = (owner[i].min(owner[j]), owner[i].max(owner[j]));
                // Along x the boundary is a line of constant x, and the portal
                // runs in z; along z it is the other way round.
                let (axis, line, at) =
                    if dx == 1 { (0u8, nx, c.z) } else { (1u8, nz, c.x) };
                let y = (c.y + n.y) * 0.5;
                spans
                    .entry((a, b, axis, line))
                    .and_modify(|s| {
                        s.lo = s.lo.min(at);
                        s.hi = s.hi.max(at);
                        s.y_sum += y;
                        s.n += 1;
                    })
                    .or_insert(PortalSpan { lo: at, hi: at, y_sum: y, n: 1 });
            }
        }
    }

    let cell = grid.cell_size;
    let mut links: Vec<Vec<Link>> = vec![Vec::new(); polys.len()];
    for ((a, b, axis, line), s) in spans {
        let y = s.y_sum / s.n as f32;
        let fixed = if axis == 0 {
            grid.origin[0] + line as f32 * cell
        } else {
            grid.origin[2] + line as f32 * cell
        };
        let lo = if axis == 0 {
            grid.origin[2] + s.lo as f32 * cell
        } else {
            grid.origin[0] + s.lo as f32 * cell
        };
        let hi = lo + (s.hi - s.lo + 1) as f32 * cell;
        let (p, q) = if axis == 0 {
            ([fixed, y, lo], [fixed, y, hi])
        } else {
            ([lo, y, fixed], [hi, y, fixed])
        };
        // Once per direction, each with its own idea of which end is the left
        // one — that is the whole reason the link is stored per direction.
        links[a].push(oriented(polys[a].centre, polys[b].centre, b, p, q));
        links[b].push(oriented(polys[b].centre, polys[a].centre, a, p, q));
    }
    // Sorted, because the portals came out of a hash map and a path that comes
    // out in a different order on a different run is a bug nobody can reproduce.
    // The tie-break on position matters for the one case where two polygons
    // share two separate boundaries.
    for l in &mut links {
        l.sort_by(|a, b| {
            a.to.cmp(&b.to)
                .then(a.left[0].total_cmp(&b.left[0]))
                .then(a.left[2].total_cmp(&b.left[2]))
        });
    }
    links
}

/// Label a portal's ends left and right for something walking `from` → `to`.
fn oriented(from: [f32; 3], to: [f32; 3], to_poly: usize, p: [f32; 3], q: [f32; 3]) -> Link {
    let d = [to[0] - from[0], to[2] - from[2]];
    let side = |v: [f32; 3]| d[0] * (v[2] - from[2]) - d[1] * (v[0] - from[0]);
    if side(p) >= side(q) {
        Link { to: to_poly, left: p, right: q }
    } else {
        Link { to: to_poly, left: q, right: p }
    }
}

fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    let (dx, dy, dz) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
    (dx * dx + dy * dy + dz * dz).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heightfield::Heightfield;
    use crate::Tri;

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

    /// The reason the whole step exists: a room of hundreds of cells has to come
    /// out as a handful of shapes, not hundreds of them.
    #[test]
    fn an_open_room_becomes_one_rectangle() {
        let mesh = bake(&slab(0.0, 0.0, 6.0, 6.0, 0.0), &open(0.5));
        assert_eq!(mesh.polys.len(), 1, "a flat floor is one rectangle: {:?}", mesh.polys.len());
        let p = mesh.polys[0];
        assert!(p.w >= 11 && p.d >= 11, "{p:?}");
        assert!(mesh.links[0].is_empty(), "one polygon has nowhere to go");
    }

    /// A bake has to be worth the money: cells in, few polygons out.
    #[test]
    fn the_cut_is_a_large_reduction_on_the_cells_it_came_from() {
        let s = open(0.25);
        let hf = Heightfield::build(&slab(0.0, 0.0, 10.0, 10.0, 0.0), &s).unwrap();
        let grid = WalkableGrid::build(&hf, &s).unwrap();
        let mesh = NavMesh::build(&grid, &s).unwrap();
        assert!(grid.cells.len() > 1500, "{}", grid.cells.len());
        assert!(
            mesh.polys.len() * 100 < grid.cells.len(),
            "{} polygons from {} cells is not a reduction worth making",
            mesh.polys.len(),
            grid.cells.len()
        );
    }

    /// Two rooms joined by a corridor: the polygons must be linked all the way
    /// through, or a path across is a path that does not exist.
    #[test]
    fn rooms_joined_by_a_corridor_are_linked_end_to_end() {
        let s = open(0.25);
        let mut tris = slab(0.0, 0.0, 3.0, 3.0, 0.0);
        tris.extend(slab(3.0, 1.0, 2.0, 1.0, 0.0));
        tris.extend(slab(5.0, 0.0, 3.0, 3.0, 0.0));
        let mesh = bake(&tris, &s);
        assert!(mesh.polys.len() > 1);
        // Everything is one region, so everything must be reachable from
        // anything: walk the links and see.
        let mut seen = vec![false; mesh.polys.len()];
        let mut stack = vec![0usize];
        seen[0] = true;
        while let Some(i) = stack.pop() {
            for l in &mesh.links[i] {
                if !seen[l.to] {
                    seen[l.to] = true;
                    stack.push(l.to);
                }
            }
        }
        assert!(seen.iter().all(|s| *s), "the links do not reach every polygon: {seen:?}");
    }

    /// Polygons in different regions must never be linked — a link across a gap
    /// nobody fits through is exactly the lie erosion exists to prevent.
    #[test]
    fn separate_islands_are_never_linked() {
        let s = open(0.5);
        let mut tris = slab(0.0, 0.0, 4.0, 4.0, 0.0);
        tris.extend(slab(20.0, 0.0, 4.0, 4.0, 0.0));
        let mesh = bake(&tris, &s);
        for (i, links) in mesh.links.iter().enumerate() {
            for l in links {
                assert_eq!(
                    mesh.polys[i].region, mesh.polys[l.to].region,
                    "polygon {i} is linked across regions"
                );
            }
        }
    }

    /// A rectangle may not claim ground that is metres away vertically, or its
    /// one height is a lie about most of it.
    #[test]
    fn a_ramp_is_cut_into_steps_rather_than_claimed_as_one_flat_shape() {
        let s = NavSettings {
            cell_size: 0.25,
            agent_radius: 0.0,
            agent_height: 1.0,
            step_height: 0.4,
            max_slope: 50.0,
        };
        // Rises 4 m over 8 m — 26°, walkable, and ten times the step height.
        let ramp = vec![
            Tri::new([0.0, 0.0, 0.0], [8.0, 4.0, 0.0], [0.0, 0.0, 3.0]),
            Tri::new([8.0, 4.0, 0.0], [8.0, 4.0, 3.0], [0.0, 0.0, 3.0]),
        ];
        let mesh = bake(&ramp, &s);
        assert!(mesh.polys.len() >= 8, "a 4 m climb in 0.4 m steps: {}", mesh.polys.len());
        for p in &mesh.polys {
            assert!(
                p.y_max - p.y_min <= s.step_height + 1e-4,
                "{p:?} spans more than one step"
            );
        }
    }

    /// A lip small enough to be one walking surface is one rectangle. Splitting
    /// on every millimetre of unevenness would make a navmesh out of a floor
    /// that a character walks across without noticing.
    #[test]
    fn a_lip_inside_one_step_stays_a_single_rectangle() {
        let s = NavSettings { step_height: 0.1, ..open(0.5) };
        let mut tris = slab(0.0, 0.0, 4.0, 4.0, 0.0);
        tris.extend(slab(4.0, 0.0, 4.0, 4.0, 0.05)); // a 5 cm lip, well inside the step
        let mesh = bake(&tris, &s);
        assert_eq!(mesh.polys.len(), 1, "{:?}", mesh.polys);
    }

    /// An L: the two arms cannot be one rectangle, so they must be two that are
    /// linked along the edge they really share.
    fn l_shaped_room(s: &NavSettings) -> NavMesh {
        let mut tris = slab(0.0, 0.0, 10.0, 3.0, 0.0);
        tris.extend(slab(0.0, 3.0, 3.0, 7.0, 0.0));
        bake(&tris, s)
    }

    /// The portal has to be the shared edge, not a guess at it: axis-aligned,
    /// long enough to walk through, and touching both rectangles.
    #[test]
    fn a_portal_lies_on_the_boundary_the_two_polygons_share() {
        let s = open(0.25);
        let mesh = l_shaped_room(&s);
        assert!(mesh.polys.len() >= 2, "an L cannot be one rectangle");
        let mut found = 0;
        for (i, links) in mesh.links.iter().enumerate() {
            for l in links {
                found += 1;
                let flat_x = (l.left[0] - l.right[0]).abs() < 1e-4;
                let flat_z = (l.left[2] - l.right[2]).abs() < 1e-4;
                assert!(flat_x != flat_z, "a portal is an axis-aligned segment: {l:?}");
                let len = if flat_x {
                    (l.left[2] - l.right[2]).abs()
                } else {
                    (l.left[0] - l.right[0]).abs()
                };
                assert!(len >= s.cell_size - 1e-4, "a portal of no width: {l:?}");
                // Its midpoint has to be on the seam — within a cell of both.
                let mid = l.midpoint();
                for p in [mesh.polys[i], mesh.polys[l.to]] {
                    let q = p.clamp(mid);
                    assert!(
                        dist(mid, q) <= s.cell_size + 1e-4,
                        "the portal is not on {p:?}'s edge: {l:?}"
                    );
                }
            }
        }
        assert!(found > 0, "the arms of an L must be linked");
    }

    /// Left and right are stated per direction, and walking back through a
    /// portal must swap them. Getting this wrong mirrors every smoothed path.
    #[test]
    fn a_portals_sides_swap_when_you_walk_back_through_it() {
        let mesh = l_shaped_room(&open(0.25));
        let a = mesh.links.iter().position(|l| !l.is_empty()).expect("some link");
        let b = mesh.links[a][0].to;
        let there = mesh.links[a].iter().find(|l| l.to == b).unwrap();
        let back = mesh.links[b].iter().find(|l| l.to == a).unwrap();
        assert_eq!(there.left, back.right);
        assert_eq!(there.right, back.left);
    }

    #[test]
    fn nearest_snaps_onto_the_mesh_and_refuses_when_it_is_too_far() {
        let mesh = bake(&slab(0.0, 0.0, 6.0, 6.0, 0.0), &open(0.5));
        let (_, on) = mesh.nearest([3.0, 1.5, 3.0], 5.0).expect("above the floor");
        assert!((on[1] - 0.0).abs() < 0.2, "it should snap down to the floor: {on:?}");
        assert!(mesh.nearest([500.0, 0.0, 500.0], 5.0).is_none(), "not on this mesh");
    }

    /// Standing on a bridge must not answer with the ground under it.
    #[test]
    fn nearest_tells_a_bridge_from_the_floor_beneath_it() {
        let s = NavSettings { cell_size: 0.5, agent_radius: 0.0, agent_height: 1.5, ..Default::default() };
        let mut tris = slab(0.0, 0.0, 6.0, 6.0, 0.0);
        tris.extend(slab(0.0, 0.0, 6.0, 6.0, 4.0));
        let mesh = bake(&tris, &s);
        let (top, _) = mesh.nearest([3.0, 4.1, 3.0], 1.0).unwrap();
        let (bottom, _) = mesh.nearest([3.0, 0.1, 3.0], 1.0).unwrap();
        assert_ne!(top, bottom);
        assert!(mesh.polys[top].y_min > mesh.polys[bottom].y_min);
    }

    /// Area is the number people will sanity-check a bake against, so it has to
    /// be the area of the floor rather than of its bounding box.
    #[test]
    fn area_measures_the_floor_it_found() {
        let mesh = bake(&slab(0.0, 0.0, 6.0, 6.0, 0.0), &open(0.25));
        assert!((mesh.area() - 36.0).abs() < 2.0, "a 6x6 floor is 36 m²: {}", mesh.area());
    }
}
