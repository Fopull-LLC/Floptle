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

use crate::filter::QueryFilter;
use crate::link::NOWHERE;
use crate::mesh::{dist, NavMesh};

/// A route: the corners to walk, in order, starting at the start and ending at
/// the goal — or at the closest reachable point to it.
#[derive(Clone, Debug, PartialEq)]
pub struct Path {
    pub points: Vec<[f32; 3]>,
    /// Whether it reaches the goal. False means the goal is on the navmesh but
    /// not reachable from the start, and these points go as near as they can.
    pub complete: bool,
    /// The legs of this route that are not walking — a ladder, a jump, a door.
    /// Empty for the ordinary path, which is most of them.
    pub crossings: Vec<Crossing>,
}

/// One leg of a path that leaves the ground: the walk from `points[at]` to
/// `points[at + 1]` is a crossing of link `link`.
///
/// A follower needs all three fields. `at` says *when*, `link` says *which* — so
/// a script can play the right animation — and `forwards` says which way round,
/// because the same ladder is climbed and descended.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Crossing {
    pub at: usize,
    pub link: u32,
    pub forwards: bool,
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

    /// The crossing that starts at this point, if the next leg is one.
    pub fn crossing_at(&self, i: usize) -> Option<Crossing> {
        self.crossings.iter().copied().find(|c| c.at == i)
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
        self.path_with(from, to, snap, &QueryFilter::default())
    }

    /// …for a character with its own rules: ground it will not walk on, ground
    /// it would rather walk on, and links it will not use.
    ///
    /// This is what lets one bake serve a whole cast. A guard who takes the road
    /// and a zombie who wades the river ask the same mesh the same question and
    /// get different routes, because the difference between them is in the
    /// question rather than in the level.
    pub fn path_with(
        &self,
        from: [f32; 3],
        to: [f32; 3],
        snap: f32,
        filter: &QueryFilter,
    ) -> Option<Path> {
        let (start, start_pos) = self.nearest_with(from, snap, filter)?;
        let (goal, goal_pos) = self.nearest_with(to, snap, filter)?;

        if start == goal {
            return Some(Path {
                points: dedupe(vec![start_pos, goal_pos]),
                complete: true,
                crossings: Vec::new(),
            });
        }

        let (corridor, complete) = self.search(start, goal, goal_pos, filter);
        // Where the goal is cut off, the walk ends on the last polygon that
        // could be reached, as near the goal as that polygon gets.
        let end = if complete {
            goal_pos
        } else {
            self.polys[corridor.last().unwrap().poly].clamp(goal_pos)
        };

        Some(self.walk_corridor(&corridor, start_pos, end, complete))
    }

    /// Turn a corridor into the points to walk, funnelling each run of ordinary
    /// ground and stopping dead at every link.
    ///
    /// A link is not a portal and must not be smoothed across: the string the
    /// funnel pulls taut runs along the floor, and a ladder is not on the floor.
    /// So each run between crossings is funnelled on its own, from where the
    /// walk enters it to the mouth of the link that ends it.
    fn walk_corridor(
        &self,
        corridor: &[Node],
        start_pos: [f32; 3],
        end: [f32; 3],
        complete: bool,
    ) -> Path {
        let mut points: Vec<[f32; 3]> = Vec::new();
        let mut crossings: Vec<Crossing> = Vec::new();
        let mut run: Vec<usize> = Vec::new();
        let mut cursor = start_pos;

        let emit = |run: &[usize], from: [f32; 3], to: [f32; 3], points: &mut Vec<[f32; 3]>| {
            let portals = self.portals_along(run, from);
            let seg = dedupe(funnel(from, to, &portals));
            if points.is_empty() {
                points.extend(seg);
            } else {
                // The first point of this run is where the last one left off.
                points.extend(seg.into_iter().skip(1));
            }
        };

        for node in corridor {
            match node.via {
                None => run.push(node.poly),
                Some((at, forwards)) => {
                    // By INDEX — the search knew exactly which link it crossed,
                    // and ids are author data (a duplicated node can repeat one;
                    // resolving by id here once spliced the wrong ladder's
                    // landing into the walk). The id is only for the caller.
                    let Some(link) = self.off_links.get(at as usize) else {
                        run.push(node.poly);
                        continue;
                    };
                    let (mouth, landing) = link.ends(forwards);
                    emit(&run, cursor, mouth, &mut points);
                    crossings.push(Crossing {
                        at: points.len().saturating_sub(1),
                        link: link.id,
                        forwards,
                    });
                    points.push(landing);
                    cursor = landing;
                    run = vec![node.poly];
                }
            }
        }
        emit(&run, cursor, end, &mut points);
        Path { points, complete, crossings }
    }

    /// A* across the polygons. Returns the corridor and whether it got there;
    /// when it did not, the corridor runs to whatever it reached that came
    /// nearest the goal.
    fn search(
        &self,
        start: usize,
        goal: usize,
        goal_pos: [f32; 3],
        filter: &QueryFilter,
    ) -> (Vec<Node>, bool) {
        let n = self.polys.len();
        let mut came: Vec<usize> = vec![usize::MAX; n];
        // How each polygon was arrived at, when it was not by walking:
        // `(off_links index, forwards)`.
        let mut via: Vec<Option<(u32, bool)>> = vec![None; n];
        let mut g: Vec<f32> = vec![f32::INFINITY; n];
        g[start] = 0.0;

        // Straight-line distance, scaled so the estimate cannot overshoot: by
        // the cheapest ground on offer, and by any usable link that beats
        // walking outright — a teleporter's flat cost across sixty metres makes
        // sixty metres of estimate an overestimate, and A* locks in the long
        // way round when its estimate overshoots.
        let mut scale = filter.cheapest(&self.areas);
        for l in &self.off_links {
            if !(l.usable(true) || l.usable(false)) || !filter.passable(l.area) {
                continue;
            }
            let (mouth, landing) = l.ends(true);
            let span = dist(mouth, landing);
            if span > 1e-3 {
                let ratio = (l.cost * filter.cost(l.area, &self.areas)) / span;
                scale = scale.min(ratio.max(0.0));
            }
        }
        let h = |i: usize| dist(self.polys[i].centre, goal_pos) * scale;
        let mut open = BinaryHeap::new();
        open.push(Ranked(h(start), start));

        // Links, indexed by the polygon they leave from — a level with a few
        // hundred of them must not be scanned once per polygon expanded.
        let leaving = self.links_from();

        // The best any polygon got, so an unreachable goal still gets a route
        // that heads towards it rather than an empty answer.
        let (mut closest, mut closest_h) = (start, h(start));

        while let Some(Ranked(f, i)) = open.pop() {
            // Lazy deletion doubling as a REOPEN: a stale entry (something
            // found `i` cheaper after this was queued) is skipped, and a node
            // improved after it was expanded simply comes round again. Links
            // make the estimate inconsistent even when it is admissible, and a
            // hard closed set under an inconsistent estimate locks in the
            // first route rather than the cheapest one.
            if f > g[i] + h(i) + 1e-4 {
                continue;
            }
            if i == goal {
                return (reconstruct(&came, &via, start, goal), true);
            }
            let hi = h(i);
            if hi < closest_h {
                closest = i;
                closest_h = hi;
            }
            let here = filter.cost(self.polys[i].area, &self.areas);
            for link in &self.links[i] {
                let j = link.to;
                if !filter.passable(self.polys[j].area) {
                    continue;
                }
                // Costed through the portal rather than centre to centre: two
                // polygons meeting at a far corner are not as close as their
                // middles make them look. Each half is priced as the ground it
                // covers, so a road that ends halfway is half price.
                let mid = link.midpoint();
                let step = dist(self.polys[i].centre, mid) * here
                    + dist(mid, self.polys[j].centre)
                        * filter.cost(self.polys[j].area, &self.areas);
                let cost = g[i] + step;
                if cost < g[j] {
                    g[j] = cost;
                    came[j] = i;
                    via[j] = None;
                    open.push(Ranked(cost + h(j), j));
                }
            }
            if let Some(range) = leaving.get(&i) {
                for &(at, forwards) in range {
                    let l = &self.off_links[at as usize];
                    if !l.usable(forwards) || !filter.passable(l.area) {
                        continue;
                    }
                    let j = l.target(forwards) as usize;
                    if j == NOWHERE as usize || !filter.passable(self.polys[j].area) {
                        continue;
                    }
                    let (mouth, landing) = l.ends(forwards);
                    // Walking to the mouth, then the crossing itself: a link
                    // that is right here is cheaper to use than one across the
                    // room, and the search has to be able to tell.
                    let cost = g[i]
                        + dist(self.polys[i].centre, mouth) * here
                        + l.cost * filter.cost(l.area, &self.areas)
                        + dist(landing, self.polys[j].centre)
                            * filter.cost(self.polys[j].area, &self.areas);
                    if cost < g[j] {
                        g[j] = cost;
                        came[j] = i;
                        via[j] = Some((at, forwards));
                        open.push(Ranked(cost + h(j), j));
                    }
                }
            }
        }
        (reconstruct(&came, &via, start, closest), false)
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

/// One step of a corridor: a polygon, and how the walk got into it. `via` is
/// `None` for the ordinary case of stepping across a shared edge.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Node {
    poly: usize,
    via: Option<(u32, bool)>,
}

fn reconstruct(
    came: &[usize],
    via: &[Option<(u32, bool)>],
    start: usize,
    end: usize,
) -> Vec<Node> {
    let mut out = vec![Node { poly: end, via: via[end] }];
    let mut at = end;
    while at != start {
        let prev = came[at];
        if prev == usize::MAX {
            break;
        }
        at = prev;
        out.push(Node { poly: at, via: via[at] });
    }
    out.reverse();
    // The first polygon was not arrived at from anywhere.
    if let Some(first) = out.first_mut() {
        first.via = None;
    }
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

    /// An axis-aligned box volume, as the inverse transform the bake wants.
    fn volume(centre: [f32; 3], half: [f32; 3], area: u8, blocks: bool) -> crate::AreaVolume {
        let mut m = [0.0f32; 16];
        for i in 0..3 {
            m[i * 5] = 1.0 / half[i];
            m[12 + i] = -centre[i] / half[i];
        }
        m[15] = 1.0;
        crate::AreaVolume { inverse: m, area, blocks }
    }

    /// The point of areas: the same two ends, and a different route, because the
    /// ground in between costs something.
    #[test]
    fn expensive_ground_is_walked_round_and_cheap_ground_is_sought_out() {
        let s = open(0.25);
        let tris = slab(0.0, 0.0, 12.0, 12.0, 0.0);
        // A band of mud across the middle third, leaving clear ground either
        // side of it.
        let mud = volume([4.0, 0.0, 6.0], [4.0, 2.0, 2.0], 1, false);
        let mesh = crate::bake_with(&tris, &s, &[mud], Vec::new())
            .unwrap()
            .with_areas(vec![crate::Area::walkable(), crate::Area::new("mud", 8.0)]);
        assert!(mesh.polys.iter().any(|p| p.area == 1), "the volume painted nothing");

        // Straight through the mud is shortest; round the end of the band is
        // cheaper. The route has to be the cheap one.
        let through = mesh.path([4.0, 0.0, 2.0], [4.0, 0.0, 10.0]).unwrap();
        assert!(through.complete);
        let in_the_mud = through
            .points
            .iter()
            .filter(|p| (4.0..8.0).contains(&p[2]))
            .map(|p| p[0])
            .fold(f32::INFINITY, f32::min);
        assert!(
            in_the_mud >= 7.9,
            "it walked into the mud instead of round its edge: {:?}",
            through.points
        );
        assert!(through.length() > 8.5, "and going round is longer: {}", through.length());

        // A character that does not mind mud takes the short way.
        let wading = QueryFilter::default().costing(1, 0.05);
        let straight = mesh.path_with([4.0, 0.0, 2.0], [4.0, 0.0, 10.0], 2.0, &wading).unwrap();
        assert!(straight.length() < through.length(), "{:?}", straight.points);
    }

    /// An excluded area is a wall as far as one character is concerned, and
    /// ordinary ground to the next.
    #[test]
    fn ground_a_character_refuses_can_cut_the_level_in_two_for_it_alone() {
        let s = open(0.25);
        let tris = slab(0.0, 0.0, 12.0, 6.0, 0.0);
        // A river all the way across.
        let river = volume([6.0, 0.0, 3.0], [1.0, 2.0, 3.5], 1, false);
        let mesh = crate::bake_with(&tris, &s, &[river], Vec::new())
            .unwrap()
            .with_areas(vec![crate::Area::walkable(), crate::Area::new("water", 2.0)]);

        let dry = QueryFilter::default().avoiding(1);
        let (a, b) = ([2.0, 0.0, 3.0], [10.0, 0.0, 3.0]);
        assert!(mesh.reachable(a, b, 2.0), "anything that will swim can cross");
        assert!(!mesh.reachable_with(a, b, 2.0, &dry), "and anything that will not, cannot");

        // …and the one that will not gets a partial path to the bank rather
        // than nothing at all.
        let stopped = mesh.path_with(a, b, 2.0, &dry).unwrap();
        assert!(!stopped.complete);
        assert!(stopped.points.last().unwrap()[0] < 5.5, "{:?}", stopped.points);
    }

    /// A carved volume is not a filter — nothing walks there, whatever it thinks
    /// of the ground.
    #[test]
    fn a_carved_volume_is_a_hole_for_everybody() {
        let s = open(0.25);
        let tris = slab(0.0, 0.0, 12.0, 6.0, 0.0);
        let keep_out = volume([6.0, 0.0, 3.0], [1.0, 2.0, 3.5], 0, true);
        let mesh = crate::bake_with(&tris, &s, &[keep_out], Vec::new()).unwrap();
        assert!(!mesh.reachable([2.0, 0.0, 3.0], [10.0, 0.0, 3.0], 2.0));
        assert!(mesh.nearest([6.0, 0.0, 3.0], 0.2).is_none(), "the middle is not ground now");
    }

    /// A paint volume overlapping a carved one must not un-carve it — blocking
    /// is a fact about the level, whatever order the scene handed the nodes in.
    #[test]
    fn a_paint_volume_cannot_uncarve_a_hole() {
        let s = open(0.25);
        let tris = slab(0.0, 0.0, 12.0, 6.0, 0.0);
        let hole = volume([6.0, 0.0, 3.0], [1.0, 2.0, 3.5], 0, true);
        // Wider than the hole and LATER in the list — the order that used to
        // resurrect every cell they share.
        let mud = volume([6.0, 0.0, 3.0], [2.0, 2.0, 3.5], 1, false);
        let mesh = crate::bake_with(&tris, &s, &[hole, mud], Vec::new())
            .unwrap()
            .with_areas(vec![crate::Area::walkable(), crate::Area::new("mud", 2.0)]);
        assert!(
            !mesh.reachable([2.0, 0.0, 3.0], [10.0, 0.0, 3.0], 2.0),
            "the hole is still a hole"
        );
        assert!(mesh.polys.iter().any(|p| p.area == 1), "the mud outside the hole painted");
    }

    /// A link that beats walking outright (a teleporter: flat cost, long span)
    /// must actually be taken — the estimate has to shrink to match it, or the
    /// search locks in the long way round and the link looks broken.
    #[test]
    fn a_cheap_link_across_a_long_room_is_taken() {
        let s = open(0.25);
        let tris = slab(0.0, 0.0, 40.0, 4.0, 0.0);
        // A cosmetic paint strip mid-room, so the floor is more than one
        // polygon and the search actually runs.
        let strip = volume([20.0, 0.0, 2.0], [0.5, 2.0, 2.5], 1, false);
        let mut portal = crate::OffLink::new(1, "portal", [1.0, 0.0, 2.0], [39.0, 0.0, 2.0]);
        portal.cost = 0.5;
        let mesh = crate::bake_with(&tris, &s, &[strip], vec![portal])
            .unwrap()
            .with_areas(vec![crate::Area::walkable(), crate::Area::new("paint", 1.0)]);
        let p = mesh.path([2.0, 0.0, 2.0], [38.0, 0.0, 2.0]).unwrap();
        assert!(p.complete);
        assert_eq!(p.crossings.len(), 1, "the portal is the cheap way: {:?}", p.points);
        // The WALKED legs are short — everything long is the crossing itself.
        let c = p.crossings[0].at;
        let walked: f32 = p
            .points
            .windows(2)
            .enumerate()
            .filter(|(i, _)| *i != c)
            .map(|(_, w)| dist(w[0], w[1]))
            .sum();
        assert!(walked < 6.0, "walk to the mouth, cross, step off: {walked}");
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
