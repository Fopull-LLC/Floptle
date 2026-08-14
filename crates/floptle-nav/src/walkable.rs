//! From "surfaces you could stand on" to "places you can walk", which is a
//! different question and takes three steps.
//!
//! 1. **Connect.** Two neighbouring cells are the same walkable surface if they
//!    are within `step_height` of each other. That is what makes a staircase one
//!    place and a ledge two.
//! 2. **Erode.** Drop every cell within `agent_radius` of an edge, so what is
//!    left is ground a body fits on rather than ground a point fits on. This is
//!    the step that stops a path scraping along a wall.
//! 3. **Group.** Flood-fill what survives into regions. A region is a set of
//!    cells you can walk between without leaving the ground.
//!
//! Erosion runs **before** grouping, deliberately. A doorway narrower than the
//! character is supposed to disappear, and if it disappears after grouping it
//! leaves one region that claims to be connected through a gap nobody fits
//! through — a path that exists right up until something tries to walk it.

use std::collections::VecDeque;

use crate::heightfield::Heightfield;
use crate::NavSettings;

/// One place you can stand: a column, and which surface within it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    pub x: usize,
    pub z: usize,
    pub y: f32,
}

/// The walkable surface, connected, eroded and grouped.
#[derive(Clone, Debug)]
pub struct WalkableGrid {
    /// Every place you can stand, **in column order** — so all the cells in one
    /// column sit next to each other, which is what lets `column_start` index
    /// them with one number instead of a list per column.
    pub cells: Vec<Cell>,
    /// `region[i]` is which region `cells[i]` belongs to. Regions are numbered
    /// from 0 and are dense.
    pub region: Vec<u32>,
    pub region_count: u32,
    pub width: usize,
    pub depth: usize,
    /// Carried through from the heightfield, because everything downstream of
    /// here has to say where a cell is in the world and a cell only knows its
    /// column.
    pub origin: [f32; 3],
    pub cell_size: f32,
    /// Where each column's cells begin in `cells`, with one extra entry on the
    /// end so a column's range is always `start[c]..start[c + 1]`.
    ///
    /// This replaced a `Vec<Vec<usize>>` — a list per column. On a 256 m level
    /// that is 2.9 million `Vec`s, nearly all of them holding one number, and
    /// building it cost more than the flood fill it existed to serve.
    column_start: Vec<u32>,
}

/// The four directions a step can go. Diagonals are deliberately excluded: a
/// diagonal move between two cells that are each beside a wall would cut the
/// corner, and the funnel that smooths the final path can produce the diagonal
/// itself when there is really room for it.
const NEIGHBOURS: [(i32, i32); 4] = [(1, 0), (-1, 0), (0, 1), (0, -1)];

impl WalkableGrid {
    /// Build from a heightfield. `None` when nothing survives — a level whose
    /// every floor is too steep, too cramped or too narrow for the agent, which
    /// is worth telling the caller rather than handing back an empty mesh.
    pub fn build(hf: &Heightfield, settings: &NavSettings) -> Option<WalkableGrid> {
        let (w, d) = (hf.width, hf.depth);
        let mut cells: Vec<Cell> = Vec::new();
        for z in 0..d {
            for x in 0..w {
                let Some(col) = hf.column(x, z) else { continue };
                for s in col.walkable(settings.agent_height) {
                    cells.push(Cell { x, z, y: s.y });
                }
            }
        }
        if cells.is_empty() {
            return None;
        }
        let mut start = column_starts(&cells, w, d);
        let step = settings.step_height;

        // One buffer, reused by every neighbour query in both sweeps below.
        // Returning a fresh `Vec` instead is one allocation per cell per visit,
        // and the flood fill visits a lot of cells.
        let mut near: Vec<usize> = Vec::with_capacity(8);

        // ---- erode -------------------------------------------------------
        // A cell is an EDGE if any of its four directions has nothing to step
        // to — the lip of a drop, the foot of a wall, the rim of a hole. Then a
        // breadth-first sweep out from every edge gives each cell its distance
        // to the nearest one, and anything closer than the agent's radius goes.
        let r = settings.radius_in_cells();
        let keep: Vec<bool> = if r <= 0 {
            vec![true; cells.len()]
        } else {
            let mut dist: Vec<i32> = vec![i32::MAX; cells.len()];
            let mut queue: VecDeque<usize> = VecDeque::new();
            for (i, slot) in dist.iter_mut().enumerate() {
                if open_directions(i, &cells, &start, w, d, step) < NEIGHBOURS.len() {
                    *slot = 0;
                    queue.push_back(i);
                }
            }
            while let Some(i) = queue.pop_front() {
                if dist[i] >= r {
                    // Already at the cutoff; nothing past it can change a
                    // decision, so stop walking that way.
                    continue;
                }
                neighbours_into(i, &cells, &start, w, d, step, &mut near);
                for &j in &near {
                    if dist[j] > dist[i] + 1 {
                        dist[j] = dist[i] + 1;
                        queue.push_back(j);
                    }
                }
            }
            dist.iter().map(|d| *d >= r).collect()
        };

        // Rebuild without the eroded cells, so indices stay dense. The order is
        // preserved, so the result is still in column order.
        if keep.iter().any(|k| !k) {
            let mut kept = Vec::with_capacity(cells.len());
            for (i, c) in cells.iter().enumerate() {
                if keep[i] {
                    kept.push(*c);
                }
            }
            cells = kept;
            if cells.is_empty() {
                return None;
            }
            start = column_starts(&cells, w, d);
        }

        // ---- group -------------------------------------------------------
        let mut region = vec![u32::MAX; cells.len()];
        let mut next = 0u32;
        let mut queue: VecDeque<usize> = VecDeque::new();
        for seed in 0..cells.len() {
            if region[seed] != u32::MAX {
                continue;
            }
            queue.clear();
            queue.push_back(seed);
            region[seed] = next;
            while let Some(i) = queue.pop_front() {
                neighbours_into(i, &cells, &start, w, d, step, &mut near);
                for &j in &near {
                    if region[j] == u32::MAX {
                        region[j] = next;
                        queue.push_back(j);
                    }
                }
            }
            next += 1;
        }

        Some(WalkableGrid {
            cells,
            region,
            region_count: next,
            width: w,
            depth: d,
            origin: hf.origin,
            cell_size: hf.cell_size,
            column_start: start,
        })
    }

    /// The cells in one column, as a range of indices into `cells`.
    ///
    /// Empty for a column with nothing to stand on, which is most of them in a
    /// level with walls in it.
    pub fn column_range(&self, x: usize, z: usize) -> std::ops::Range<usize> {
        if x >= self.width || z >= self.depth {
            return 0..0;
        }
        let c = z * self.width + x;
        self.column_start[c] as usize..self.column_start[c + 1] as usize
    }

    /// How many cells are in each region, largest first. The shape of a bake in
    /// one line: one big number is a level you can cross, a scatter of small
    /// ones is a level cut into islands.
    pub fn region_sizes(&self) -> Vec<usize> {
        let mut counts = vec![0usize; self.region_count as usize];
        for r in &self.region {
            counts[*r as usize] += 1;
        }
        counts.sort_unstable_by(|a, b| b.cmp(a));
        counts
    }
}

/// Where each column's cells begin, as a running total. Relies on `cells` being
/// in column order, which is how they are built and how erosion leaves them.
fn column_starts(cells: &[Cell], width: usize, depth: usize) -> Vec<u32> {
    let mut start = vec![0u32; width * depth + 1];
    for c in cells {
        start[c.z * width + c.x + 1] += 1;
    }
    for i in 1..start.len() {
        start[i] += start[i - 1];
    }
    start
}

/// Every cell you can step to from `i`, written into `out`.
fn neighbours_into(
    i: usize,
    cells: &[Cell],
    start: &[u32],
    width: usize,
    depth: usize,
    step: f32,
    out: &mut Vec<usize>,
) {
    out.clear();
    let c = cells[i];
    for (dx, dz) in NEIGHBOURS {
        let (nx, nz) = (c.x as i32 + dx, c.z as i32 + dz);
        if nx < 0 || nz < 0 || nx as usize >= width || nz as usize >= depth {
            continue;
        }
        let col = nz as usize * width + nx as usize;
        let from = start[col] as usize;
        for (k, other) in cells[from..start[col + 1] as usize].iter().enumerate() {
            if (other.y - c.y).abs() <= step {
                out.push(from + k);
            }
        }
    }
}

/// How many of the four directions have somewhere to step to. Fewer than all
/// four makes the cell an edge, and edges are what erosion measures from.
///
/// Counting **directions** rather than neighbours matters where a column holds
/// two surfaces: a cell at the lip of a hole beside a bridge can have two
/// neighbours to one side and none to another, which totals four and reads as
/// surrounded when it is standing on the brink.
fn open_directions(
    i: usize,
    cells: &[Cell],
    start: &[u32],
    width: usize,
    depth: usize,
    step: f32,
) -> usize {
    let c = cells[i];
    let mut open = 0;
    for (dx, dz) in NEIGHBOURS {
        let (nx, nz) = (c.x as i32 + dx, c.z as i32 + dz);
        if nx < 0 || nz < 0 || nx as usize >= width || nz as usize >= depth {
            continue;
        }
        let col = nz as usize * width + nx as usize;
        if (start[col] as usize..start[col + 1] as usize)
            .any(|j| (cells[j].y - c.y).abs() <= step)
        {
            open += 1;
        }
    }
    open
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tri;

    /// A `w`×`d` floor at height `y`, as two triangles.
    fn slab(x0: f32, z0: f32, w: f32, d: f32, y: f32) -> Vec<Tri> {
        vec![
            Tri::new([x0, y, z0], [x0 + w, y, z0], [x0, y, z0 + d]),
            Tri::new([x0 + w, y, z0], [x0 + w, y, z0 + d], [x0, y, z0 + d]),
        ]
    }

    fn grid(tris: &[Tri], s: &NavSettings) -> Option<WalkableGrid> {
        let hf = Heightfield::build(tris, s).unwrap();
        WalkableGrid::build(&hf, s)
    }

    #[test]
    fn one_open_floor_is_one_region() {
        let s = NavSettings { cell_size: 0.5, agent_radius: 0.0, ..Default::default() };
        let g = grid(&slab(0.0, 0.0, 6.0, 6.0, 0.0), &s).unwrap();
        assert_eq!(g.region_count, 1);
        assert!(g.cells.len() > 100);
    }

    /// Erosion is the whole reason a path can be followed by something with a
    /// body, so it has to actually remove the rim.
    #[test]
    fn the_agent_radius_eats_into_the_edges() {
        let wide = NavSettings { cell_size: 0.5, agent_radius: 0.0, ..Default::default() };
        let body = NavSettings { cell_size: 0.5, agent_radius: 1.0, ..Default::default() };
        let tris = slab(0.0, 0.0, 6.0, 6.0, 0.0);
        let open = grid(&tris, &wide).unwrap();
        let eroded = grid(&tris, &body).unwrap();
        assert!(
            eroded.cells.len() < open.cells.len(),
            "{} should be fewer than {}",
            eroded.cells.len(),
            open.cells.len()
        );
        assert_eq!(eroded.region_count, 1, "eroding must not shatter an open floor");
    }

    /// A floor smaller than the character is not somewhere it can stand, and
    /// saying so is better than offering a path onto it.
    #[test]
    fn ground_narrower_than_the_agent_survives_nothing() {
        let s = NavSettings { cell_size: 0.25, agent_radius: 2.0, ..Default::default() };
        assert!(grid(&slab(0.0, 0.0, 1.0, 1.0, 0.0), &s).is_none());
    }

    /// Two floors far apart vertically are two places, whatever their footprint
    /// looks like from above.
    #[test]
    fn a_ledge_out_of_step_range_is_a_second_region() {
        let s = NavSettings {
            cell_size: 0.5,
            agent_radius: 0.0,
            step_height: 0.4,
            agent_height: 1.0,
            ..Default::default()
        };
        let mut tris = slab(0.0, 0.0, 4.0, 4.0, 0.0);
        tris.extend(slab(5.0, 0.0, 4.0, 4.0, 3.0)); // beside it, and well above
        let g = grid(&tris, &s).unwrap();
        assert_eq!(g.region_count, 2);
    }

    /// …and a step small enough to walk up is the same place. This is the pair
    /// that makes `step_height` mean something: the same geometry, one number
    /// apart, has to answer differently.
    #[test]
    fn a_lip_within_step_range_stays_one_region() {
        let s = NavSettings {
            cell_size: 0.5,
            agent_radius: 0.0,
            step_height: 0.4,
            agent_height: 1.0,
            ..Default::default()
        };
        let mut tris = slab(0.0, 0.0, 4.0, 4.0, 0.0);
        tris.extend(slab(4.0, 0.0, 4.0, 4.0, 0.3)); // touching, 30 cm up
        let g = grid(&tris, &s).unwrap();
        assert_eq!(g.region_count, 1, "a 30 cm step under a 40 cm limit is one place");

        let low = NavSettings { step_height: 0.2, ..s };
        let g2 = grid(&tris, &low).unwrap();
        assert_eq!(g2.region_count, 2, "the same step over a 20 cm limit is two");
    }

    /// The reason erosion runs before grouping: a doorway nobody fits through
    /// must not leave behind a region that claims to be connected.
    #[test]
    fn a_gap_too_narrow_for_the_agent_separates_the_rooms() {
        let s = NavSettings {
            cell_size: 0.25,
            agent_radius: 0.6,
            agent_height: 1.0,
            ..Default::default()
        };
        // Two rooms joined by a corridor 0.5 m wide — wider than a cell, far
        // narrower than a 1.2 m-wide character.
        let mut tris = slab(0.0, 0.0, 3.0, 3.0, 0.0);
        tris.extend(slab(3.0, 1.25, 2.0, 0.5, 0.0));
        tris.extend(slab(5.0, 0.0, 3.0, 3.0, 0.0));
        let g = grid(&tris, &s).unwrap();
        assert_eq!(g.region_count, 2, "the corridor must not connect them");

        // The same corridor with a character that fits walks straight through.
        // Note the cell size comes down with the radius: erosion is quantised
        // to whole columns, so a cell coarse next to the radius eats far more
        // than the radius asked for. `cell_size_advice` says so out loud; here
        // the numbers are simply chosen well.
        let small =
            NavSettings { agent_radius: 0.15, cell_size: 0.05, ..s };
        assert!(crate::cell_size_advice(&small).is_none(), "the test's own numbers must be sound");
        let g2 = grid(&tris, &small).unwrap();
        assert_eq!(g2.region_count, 1, "a 0.5 m corridor fits a 0.3 m-wide character");
    }

    /// A box, wound outward — a wall as the modelling tool, the mesh importer
    /// and the box primitive all hand one over.
    fn wall(lo: [f32; 3], hi: [f32; 3]) -> Vec<Tri> {
        let v = |i: usize| {
            [
                if i & 1 == 0 { lo[0] } else { hi[0] },
                if i & 2 == 0 { lo[1] } else { hi[1] },
                if i & 4 == 0 { lo[2] } else { hi[2] },
            ]
        };
        const QUADS: [[usize; 4]; 6] = [
            [0, 1, 3, 2],
            [4, 5, 7, 6],
            [0, 1, 5, 4], // -y, looking down
            [2, 6, 7, 3], // +y, looking up
            [0, 2, 6, 4],
            [1, 3, 7, 5],
        ];
        let mut out = Vec::new();
        for q in QUADS {
            out.push(Tri::new(v(q[0]), v(q[1]), v(q[2])));
            out.push(Tri::new(v(q[0]), v(q[2]), v(q[3])));
        }
        out
    }

    /// Which region the floor belongs to at a spot, or `None` if there is no
    /// floor there at all.
    fn floor_region(g: &WalkableGrid, x: f32, z: f32) -> Option<u32> {
        let cx = ((x - g.origin[0]) / g.cell_size) as usize;
        let cz = ((z - g.origin[2]) / g.cell_size) as usize;
        g.column_range(cx, cz).find(|i| g.cells[*i].y < 1.0).map(|i| g.region[i])
    }

    /// **The report this was rewritten for.** A wall built across a room has to
    /// separate the room, wherever it happens to sit against the bake grid and
    /// however thin it is. A wall that blocks at some offsets and not others is
    /// "it noticed part of it and skipped the rest".
    #[test]
    fn a_wall_across_a_room_separates_it_at_any_thickness_or_offset() {
        let s = NavSettings {
            cell_size: 0.15,
            agent_radius: 0.0,
            agent_height: 1.8,
            step_height: 0.4,
            ..Default::default()
        };
        for thickness in [0.04, 0.1, 0.3, 1.0] {
            for offset in [0.0, 0.037, 0.075, 0.11] {
                let x0 = 3.0 + offset;
                let mut tris = slab(0.0, 0.0, 6.0, 6.0, 0.0);
                tris.extend(wall([x0, 0.0, 0.0], [x0 + thickness, 2.5, 6.0]));
                let g = grid(&tris, &s).unwrap();
                let here = floor_region(&g, 1.5, 3.0);
                let there = floor_region(&g, 5.0, 3.0);
                assert!(here.is_some() && there.is_some(), "the room must survive: {x0}");
                assert_ne!(
                    here, there,
                    "a {thickness} m wall at x = {x0} left both sides of the room joined"
                );
            }
        }
    }

    /// …and a doorway in that wall joins it back up, so blocking is the geometry
    /// talking rather than a wall-shaped region being deleted.
    #[test]
    fn a_doorway_in_the_wall_joins_the_room_back_up() {
        let s = NavSettings {
            cell_size: 0.15,
            agent_radius: 0.3,
            agent_height: 1.8,
            step_height: 0.4,
            ..Default::default()
        };
        let mut tris = slab(0.0, 0.0, 6.0, 6.0, 0.0);
        // The same wall in two pieces, with 1.4 m of air between them.
        tris.extend(wall([3.0, 0.0, 0.0], [3.2, 2.5, 2.3]));
        tris.extend(wall([3.0, 0.0, 3.7], [3.2, 2.5, 6.0]));
        let g = grid(&tris, &s).unwrap();
        assert_eq!(
            floor_region(&g, 1.5, 3.0),
            floor_region(&g, 5.0, 3.0),
            "a 1.4 m doorway fits a 0.6 m-wide character"
        );

        // Brick it up and the room is two rooms again.
        let mut shut = slab(0.0, 0.0, 6.0, 6.0, 0.0);
        shut.extend(wall([3.0, 0.0, 0.0], [3.2, 2.5, 6.0]));
        let g = grid(&shut, &s).unwrap();
        assert_ne!(floor_region(&g, 1.5, 3.0), floor_region(&g, 5.0, 3.0));
    }

    /// A wall you can see over is still a wall you cannot walk through, and a
    /// kerb is still a kerb — the same geometry an inch apart has to answer
    /// differently, or `step_height` means nothing.
    #[test]
    fn a_lip_is_stepped_over_and_a_wall_is_not() {
        let s = NavSettings {
            cell_size: 0.15,
            agent_radius: 0.0,
            agent_height: 1.8,
            step_height: 0.4,
            ..Default::default()
        };
        let mut kerb = slab(0.0, 0.0, 6.0, 6.0, 0.0);
        kerb.extend(wall([3.0, 0.0, 0.0], [3.3, 0.3, 6.0]));
        let g = grid(&kerb, &s).unwrap();
        assert_eq!(
            floor_region(&g, 1.5, 3.0),
            floor_region(&g, 5.0, 3.0),
            "a 30 cm kerb under a 40 cm step height is walked over"
        );

        let mut low_wall = slab(0.0, 0.0, 6.0, 6.0, 0.0);
        low_wall.extend(wall([3.0, 0.0, 0.0], [3.3, 0.6, 6.0]));
        let g = grid(&low_wall, &s).unwrap();
        assert_ne!(floor_region(&g, 1.5, 3.0), floor_region(&g, 5.0, 3.0), "a 60 cm wall is not");
    }

    #[test]
    fn region_sizes_come_back_largest_first() {
        let s = NavSettings {
            cell_size: 0.5,
            agent_radius: 0.0,
            agent_height: 1.0,
            ..Default::default()
        };
        let mut tris = slab(0.0, 0.0, 6.0, 6.0, 0.0);
        tris.extend(slab(20.0, 0.0, 2.0, 2.0, 0.0));
        let g = grid(&tris, &s).unwrap();
        let sizes = g.region_sizes();
        assert_eq!(sizes.len(), 2);
        assert!(sizes[0] > sizes[1], "{sizes:?}");
    }
}
