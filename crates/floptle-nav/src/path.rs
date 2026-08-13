//! Getting from here to there — A* over the polygons, then a funnel to pull the
//! result straight.
//!
//! The search is the easy half and the funnel is the half that matters. A* gives
//! a **corridor**: the polygons to cross, in order. Walking their centres would
//! be a path that visibly zigzags between the middles of rooms, which is the
//! look of pathfinding rather than the look of walking. The funnel takes that
//! corridor and pulls a string taut through it, so the result bends only where
//! it has to bend — at a corner it actually has to go round.
//!
//! It is the classic "simple stupid funnel": carry a left and a right edge out
//! from the last corner, narrow them as the portals allow, and the moment they
//! cross each other, the one they crossed over is a corner and becomes the next
//! apex. One pass, no smoothing pass afterwards, no path that cuts a wall.
//!
//! # When there is no way through
//!
//! Two things can go wrong, and they are different, so they are answered
//! differently. Somewhere that is not on the navmesh at all gets `None` — the
//! question was about a place this mesh does not cover. Somewhere on the mesh
//! but cut off gets a path with [`Path::complete`] false, running as close to
//! the goal as the ground allows. A character that walks to the near side of a
//! chasm and stops is behaving; one that stands still because the answer was
//! empty looks broken.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::mesh::NavMesh;

/// A route: the corners to walk, in order, starting at the start and ending at
/// the goal — or at the closest reachable point to it.
#[derive(Clone, Debug, PartialEq)]
pub struct Path {
    pub points: Vec<[f32; 3]>,
    /// Whether it reaches the goal. False means the goal is on the navmesh but
    /// not reachable from the start, and these points go as near as they can.
    pub complete: bool,
}

impl Path {
    /// How far it is to walk, in metres.
    pub fn length(&self) -> f32 {
        self.points.windows(2).map(|w| dist(w[0], w[1])).sum()
    }

    /// How many corners it turns. A straight walk across a room is 0.
    pub fn corners(&self) -> usize {
        self.points.len().saturating_sub(2)
    }
}

impl NavMesh {
    /// A path between two world points.
    ///
    /// Both ends are snapped onto the navmesh first, within about a character's
    /// height — so standing on top of the floor, or half a step off the edge of
    /// it, is the ordinary case it is meant to handle rather than a failure.
    /// `None` means an end was nowhere near the mesh.
    pub fn path(&self, from: [f32; 3], to: [f32; 3]) -> Option<Path> {
        self.path_within(from, to, self.settings.agent_height)
    }

    /// [`NavMesh::path`], with your own idea of how far off the mesh an end may
    /// be. Useful when asking about a point that is deliberately loose — a click
    /// in the world, say, where the answer should be "the nearest walkable
    /// thing" rather than nothing.
    pub fn path_within(&self, from: [f32; 3], to: [f32; 3], snap: f32) -> Option<Path> {
        let (start, start_pos) = self.nearest(from, snap)?;
        let (goal, goal_pos) = self.nearest(to, snap)?;

        if start == goal {
            return Some(Path { points: dedupe(vec![start_pos, goal_pos]), complete: true });
        }

        let (corridor, complete) = self.search(start, goal, goal_pos);
        // Where the goal is cut off, the walk ends on the last polygon that
        // could be reached, as near the goal as that polygon gets.
        let end = if complete {
            goal_pos
        } else {
            self.polys[*corridor.last().unwrap()].clamp(goal_pos)
        };

        let portals = self.portals_along(&corridor, start_pos);
        Some(Path { points: dedupe(funnel(start_pos, end, &portals)), complete })
    }

    /// A* across the polygons. Returns the corridor and whether it got there;
    /// when it did not, the corridor runs to whatever it reached that came
    /// nearest the goal.
    fn search(&self, start: usize, goal: usize, goal_pos: [f32; 3]) -> (Vec<usize>, bool) {
        let n = self.polys.len();
        let mut came: Vec<usize> = vec![usize::MAX; n];
        let mut g: Vec<f32> = vec![f32::INFINITY; n];
        let mut done = vec![false; n];
        g[start] = 0.0;

        let h = |i: usize| dist(self.polys[i].centre, goal_pos);
        let mut open = BinaryHeap::new();
        open.push(Ranked(h(start), start));

        // The best any polygon got, so an unreachable goal still gets a route
        // that heads towards it rather than an empty answer.
        let (mut closest, mut closest_h) = (start, h(start));

        while let Some(Ranked(_, i)) = open.pop() {
            if done[i] {
                continue;
            }
            done[i] = true;
            if i == goal {
                return (reconstruct(&came, start, goal), true);
            }
            let hi = h(i);
            if hi < closest_h {
                closest = i;
                closest_h = hi;
            }
            for link in &self.links[i] {
                let j = link.to;
                // Costed through the portal rather than centre to centre: two
                // polygons meeting at a far corner are not as close as their
                // middles make them look.
                let step = dist(self.polys[i].centre, link.midpoint())
                    + dist(link.midpoint(), self.polys[j].centre);
                let cost = g[i] + step;
                if cost < g[j] {
                    g[j] = cost;
                    came[j] = i;
                    open.push(Ranked(cost + h(j), j));
                }
            }
        }
        (reconstruct(&came, start, closest), false)
    }

    /// The portals to cross along a corridor, as (left, right) pairs.
    fn portals_along(&self, corridor: &[usize], start: [f32; 3]) -> Vec<([f32; 3], [f32; 3])> {
        let mut out = Vec::with_capacity(corridor.len());
        let mut cursor = start;
        for pair in corridor.windows(2) {
            // Two polygons can share more than one boundary — one passing over
            // the other — so take the crossing nearest to where we are, not the
            // first one that names the right polygon.
            let Some(link) = self.links[pair[0]]
                .iter()
                .filter(|l| l.to == pair[1])
                .min_by(|a, b| dist(cursor, a.midpoint()).total_cmp(&dist(cursor, b.midpoint())))
            else {
                continue;
            };
            cursor = link.midpoint();
            out.push((link.left, link.right));
        }
        out
    }
}

/// A polygon and its score, ordered so the heap hands back the smallest.
///
/// The tie-break on index is not decoration: two polygons scoring identically is
/// ordinary on a grid-derived mesh, and without it the route depends on which
/// one the heap happened to keep.
struct Ranked(f32, usize);

impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Ranked {}
impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        other.0.total_cmp(&self.0).then(other.1.cmp(&self.1))
    }
}
impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn reconstruct(came: &[usize], start: usize, end: usize) -> Vec<usize> {
    let mut out = vec![end];
    let mut at = end;
    while at != start {
        at = came[at];
        if at == usize::MAX {
            break;
        }
        out.push(at);
    }
    out.reverse();
    out
}

/// Twice the signed area of the triangle abc, in plan. Positive when c is to the
/// left of the line a→b, which is the one convention the funnel and the portal
/// orientation in [`crate::mesh`] both have to agree on.
fn side(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> f32 {
    (b[0] - a[0]) * (c[2] - a[2]) - (b[2] - a[2]) * (c[0] - a[0])
}

fn same(a: [f32; 3], b: [f32; 3]) -> bool {
    (a[0] - b[0]).abs() < 1e-5 && (a[2] - b[2]).abs() < 1e-5
}

fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    let (dx, dy, dz) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
    (dx * dx + dy * dy + dz * dz).sqrt()
}

fn dedupe(points: Vec<[f32; 3]>) -> Vec<[f32; 3]> {
    let mut out: Vec<[f32; 3]> = Vec::with_capacity(points.len());
    for p in points {
        if out.last().is_none_or(|q| !same(*q, p) || (q[1] - p[1]).abs() > 1e-4) {
            out.push(p);
        }
    }
    out
}

/// Pull a string taut through a corridor of portals.
fn funnel(start: [f32; 3], end: [f32; 3], portals: &[([f32; 3], [f32; 3])]) -> Vec<[f32; 3]> {
    let mut gates: Vec<([f32; 3], [f32; 3])> = Vec::with_capacity(portals.len() + 1);
    gates.extend_from_slice(portals);
    // The goal closes the funnel: a portal with no width, which nothing can pass
    // either side of.
    gates.push((end, end));

    let mut out = vec![start];
    let (mut apex, mut left, mut right) = (start, start, start);
    let (mut left_i, mut right_i) = (0usize, 0usize);

    let mut i = 0;
    while i < gates.len() {
        let (l, r) = gates[i];

        // Narrow the right edge if the new one is inside it.
        if side(apex, right, r) >= 0.0 {
            if same(apex, right) || side(apex, left, r) < 0.0 {
                right = r;
                right_i = i;
            } else {
                // The right edge has swung past the left: the left one was a
                // corner all along. Everything restarts from there — the apex
                // moves to the corner and the scan goes back to the portal that
                // put it there, because the portals in between were measured
                // against an apex that is no longer where the walk turns.
                out.push(left);
                apex = left;
                let corner = left_i;
                left = apex;
                right = apex;
                left_i = corner;
                right_i = corner;
                i = corner + 1;
                continue;
            }
        }

        // And the mirror of that for the left edge.
        if side(apex, left, l) <= 0.0 {
            if same(apex, left) || side(apex, right, l) > 0.0 {
                left = l;
                left_i = i;
            } else {
                out.push(right);
                apex = right;
                let corner = right_i;
                left = apex;
                right = apex;
                left_i = corner;
                right_i = corner;
                i = corner + 1;
                continue;
            }
        }

        i += 1;
    }

    out.push(end);
    out
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

    /// The whole point of the funnel: nothing in the way means a straight line,
    /// not a tour of polygon centres.
    #[test]
    fn across_an_empty_room_is_a_straight_line() {
        let mesh = bake(&slab(0.0, 0.0, 10.0, 10.0, 0.0), &open(0.25));
        let p = mesh.path([1.0, 0.0, 1.0], [9.0, 0.0, 9.0]).unwrap();
        assert!(p.complete);
        assert_eq!(p.corners(), 0, "a clear room needs no corners: {:?}", p.points);
        let straight = dist([1.0, 0.0, 1.0], [9.0, 0.0, 9.0]);
        assert!(
            (p.length() - straight).abs() < 0.3,
            "{} should be about {straight}",
            p.length()
        );
    }

    /// An L-shaped room: exactly one corner, and it must be at the inside of the
    /// bend rather than out in the middle of the floor.
    #[test]
    fn an_l_shaped_room_turns_once_at_the_inside_corner() {
        let s = open(0.25);
        // A 10x3 arm along x, and a 3x10 arm along z, meeting at the origin.
        let mut tris = slab(0.0, 0.0, 10.0, 3.0, 0.0);
        tris.extend(slab(0.0, 3.0, 3.0, 7.0, 0.0));
        let mesh = bake(&tris, &s);
        let p = mesh.path([9.0, 0.0, 1.5], [1.5, 0.0, 9.0]).unwrap();
        assert!(p.complete);
        assert_eq!(p.corners(), 1, "one bend, one corner: {:?}", p.points);
        let corner = p.points[1];
        // The inside of the bend is around (3, 3): the corner must hug it.
        assert!(
            dist(corner, [3.0, 0.0, 3.0]) < 1.0,
            "the corner should hug the inside of the bend, not the middle: {corner:?}"
        );
        // And it must be shorter than going via the middle of the two arms.
        assert!(p.length() < 16.0, "{}", p.length());
    }

    /// A path must go through the doorway, not through the wall beside it.
    #[test]
    fn a_path_between_rooms_goes_through_the_door() {
        let s = open(0.25);
        let mut tris = slab(0.0, 0.0, 5.0, 5.0, 0.0);
        tris.extend(slab(5.0, 2.0, 1.0, 1.0, 0.0)); // the doorway
        tris.extend(slab(6.0, 0.0, 5.0, 5.0, 0.0));
        let mesh = bake(&tris, &s);
        let p = mesh.path([1.0, 0.0, 1.0], [10.0, 0.0, 1.0]).unwrap();
        assert!(p.complete, "{p:?}");
        // Every leg of the walk has to stay on the floor.
        for w in p.points.windows(2) {
            for t in 0..=10 {
                let f = t as f32 / 10.0;
                let at = [
                    w[0][0] + (w[1][0] - w[0][0]) * f,
                    0.0,
                    w[0][2] + (w[1][2] - w[0][2]) * f,
                ];
                assert!(
                    mesh.nearest(at, 0.3).is_some(),
                    "the path leaves the floor at {at:?}: {:?}",
                    p.points
                );
            }
        }
        assert!(p.length() > 9.0, "it has to detour through the door: {}", p.length());
    }

    /// Somewhere cut off is a different answer from somewhere that does not
    /// exist, and a character should still walk to the edge of the chasm.
    #[test]
    fn an_unreachable_goal_gives_a_partial_path_rather_than_nothing() {
        let s = open(0.5);
        let mut tris = slab(0.0, 0.0, 4.0, 4.0, 0.0);
        tris.extend(slab(20.0, 0.0, 4.0, 4.0, 0.0));
        let mesh = bake(&tris, &s);
        let p = mesh.path([1.0, 0.0, 1.0], [22.0, 0.0, 2.0]).unwrap();
        assert!(!p.complete, "the far island is not reachable");
        assert!(p.points.len() >= 2);
        let end = *p.points.last().unwrap();
        assert!(end[0] < 5.0, "it should stop on the near island: {end:?}");
        // …and it should have walked towards the goal, not stood still.
        assert!(p.length() > 1.0, "{:?}", p.points);
    }

    /// Off the mesh entirely is a question this mesh cannot answer, and saying
    /// so is better than a path from a place that does not exist.
    #[test]
    fn a_point_nowhere_near_the_mesh_has_no_path() {
        let mesh = bake(&slab(0.0, 0.0, 4.0, 4.0, 0.0), &open(0.5));
        assert!(mesh.path([1.0, 0.0, 1.0], [500.0, 0.0, 500.0]).is_none());
        assert!(mesh.path([500.0, 0.0, 500.0], [1.0, 0.0, 1.0]).is_none());
    }

    /// Standing a bit above the floor is the normal case, not an error.
    #[test]
    fn the_ends_snap_onto_the_floor() {
        let mesh = bake(&slab(0.0, 0.0, 6.0, 6.0, 0.0), &open(0.5));
        let p = mesh.path([1.0, 0.9, 1.0], [5.0, 0.9, 5.0]).unwrap();
        assert!(p.complete);
        for pt in &p.points {
            assert!(pt[1].abs() < 0.1, "the walk should be on the floor: {pt:?}");
        }
    }

    #[test]
    fn a_path_to_where_you_already_are_is_two_points_at_most() {
        let mesh = bake(&slab(0.0, 0.0, 6.0, 6.0, 0.0), &open(0.5));
        let p = mesh.path([3.0, 0.0, 3.0], [3.0, 0.0, 3.0]).unwrap();
        assert_eq!(p.points.len(), 1, "{:?}", p.points);
        assert_eq!(p.length(), 0.0);
        assert!(p.complete);
    }

    /// The same question must give the same answer. Portals come out of a hash
    /// map, and unsorted they would make a route that changes between runs.
    #[test]
    fn the_same_question_answers_the_same_way_every_time() {
        let s = open(0.25);
        let mut tris = slab(0.0, 0.0, 6.0, 3.0, 0.0);
        tris.extend(slab(0.0, 3.0, 3.0, 6.0, 0.0));
        let first = bake(&tris, &s).path([5.0, 0.0, 1.0], [1.0, 0.0, 8.0]).unwrap();
        for _ in 0..4 {
            let again = bake(&tris, &s).path([5.0, 0.0, 1.0], [1.0, 0.0, 8.0]).unwrap();
            assert_eq!(first, again);
        }
    }

    /// A path up a ramp has to climb it — the y of the walk must follow the
    /// ground rather than staying at the height it started.
    #[test]
    fn a_path_up_a_ramp_climbs() {
        let s = NavSettings {
            cell_size: 0.25,
            agent_radius: 0.0,
            agent_height: 1.0,
            max_slope: 50.0,
            ..Default::default()
        };
        let ramp = vec![
            Tri::new([0.0, 0.0, 0.0], [8.0, 4.0, 0.0], [0.0, 0.0, 3.0]),
            Tri::new([8.0, 4.0, 0.0], [8.0, 4.0, 3.0], [0.0, 0.0, 3.0]),
        ];
        let mesh = bake(&ramp, &s);
        let p = mesh.path([0.5, 0.0, 1.5], [7.5, 4.0, 1.5]).unwrap();
        assert!(p.complete);
        let end = *p.points.last().unwrap();
        assert!(end[1] > 3.0, "it should end up the top: {end:?}");
        // Climbing is longer than the ground it covers.
        assert!(p.length() > 7.5, "{}", p.length());
    }
}
