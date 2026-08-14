//! The heightfield — the level, sampled into columns.
//!
//! Every triangle is rasterised into the square columns its footprint **overlaps**,
//! and each column keeps a sorted stack of the solids it hit. That stack is what
//! makes a floor under a bridge a different place from the bridge: two surfaces
//! in one column, each with its own headroom.
//!
//! # Ground is a height; a wall is a range
//!
//! The two kinds of triangle are recorded differently, because they answer
//! different questions.
//!
//! **Ground** — anything within `max_slope` of flat — records one height. Where
//! the column's centre is on the triangle that height is read off it exactly,
//! which is what keeps a ramp a ramp rather than a staircase; on a column the
//! triangle only clips, it comes off the same plane, extended. Ground narrower
//! than a column used to fall between centres and vanish — a catwalk, a kerb,
//! the lip of a step — and does not now.
//!
//! **A wall is not a height at all.** A vertical face projects to a line in plan:
//! it has no area, contains no column centre, and sampling could never see it. A
//! wall recorded that way blocks nothing, and a room built out of walls bakes as
//! though the walls were not there — which is the bug this design exists to not
//! have. So a face too steep to stand on is rasterised by **overlap** — every
//! column its footprint touches, however thin — and records the **span of solid**
//! it occupies there, foot to top. A span with a floor inside it swallows the
//! floor, which is what "you cannot walk through a wall" means to a column.
//!
//! That is why [`Surface`] carries a `base` as well as a `y`. Ground is a surface
//! with no thickness and the two are equal; a wall's `base` is its foot.
//!
//! # What is still an approximation
//!
//! A column is claimed if any part of the triangle falls in it, so geometry is
//! rounded **outward** to the grid — up to half a cell of wall where there is
//! only air. That is the safe direction (the alternative is a path through a
//! wall) and erosion by the agent radius already removes more than it adds.
//!
//! Winding is deliberately not trusted: `|normal.y|` decides slope, so a floor
//! whose triangles face down is still a floor. The cost is that the inside of a
//! solid thicker than the agent is not filled in — its perimeter blocks, its
//! middle reads as ground. That ground is enclosed by the perimeter, so nothing
//! can reach it and no path crosses it; it is a stray patch in the overlay
//! rather than a route through a wall.

use crate::{NavSettings, Tri};

/// One solid in a column: what you would stand on, and what it stands on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Surface {
    /// The top — the height you would be at, standing here.
    pub y: f32,
    /// The bottom of the same solid. Equal to `y` for ground, which has no
    /// thickness; for a wall it is its foot, and everything between the two is
    /// somewhere you are not.
    pub base: f32,
    /// Not too steep. Says nothing about headroom — [`Column::walkable`] adds
    /// that, because clearance depends on what is above and this does not know
    /// yet.
    pub flat_enough: bool,
}

impl Surface {
    /// Ground: a top with nothing under it.
    fn ground(y: f32, flat_enough: bool) -> Surface {
        Surface { y, base: y, flat_enough }
    }
}

/// The surfaces in one column, lowest first.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Column {
    pub surfaces: Vec<Surface>,
    /// Heights of ground in this column facing **down** — undersides. Scratch,
    /// emptied by [`Column::merge`]; see [`Column::fill_solids`].
    undersides: Vec<f32>,
    /// Heights of ground in this column facing **up**. Scratch, as above.
    uppers: Vec<f32>,
}

impl Column {
    /// Surfaces you could actually stand on: flat enough, **and** with at least
    /// `agent_height` of nothing above them.
    ///
    /// The ceiling is the **foot** of the next solid up, whatever it is — a
    /// walkable floor above you is still a ceiling, and a wall's underside is
    /// its foot rather than its top. Measuring to the top instead would give a
    /// three-metre wall three metres of headroom under it.
    pub fn walkable(&self, agent_height: f32) -> Vec<Surface> {
        self.surfaces
            .iter()
            .enumerate()
            .filter(|(i, s)| {
                if !s.flat_enough {
                    return false;
                }
                match self.surfaces.get(i + 1) {
                    Some(above) => above.base - s.y >= agent_height,
                    // Nothing above it at all: open sky.
                    None => true,
                }
            })
            .map(|(_, s)| *s)
            .collect()
    }

    /// Fill in the inside of a solid, from the only evidence there is: which way
    /// its horizontal faces look.
    ///
    /// A wall's vertical sides give it a span, so a wall up to a couple of
    /// columns thick is solid all the way through. A **thick** one has columns
    /// with no side face in them at all — only the box's underside at the bottom
    /// and its top at the top — and the gap between them is exactly as
    /// indistinguishable from a room as it sounds. Ground facing **down** is the
    /// difference: a room's floor looks up and its ceiling looks down, in that
    /// order; a pillar's underside looks down and its top looks up, in that
    /// order. So an underside with ground above it looking up encloses solid,
    /// and an underside with nothing above it is a ceiling.
    ///
    /// This only ever **adds** an obstruction. Geometry whose winding says
    /// nothing useful — a single-sided plane, a mesh built inside out — falls
    /// through it unchanged rather than losing its floor, which is why slope is
    /// still judged on `|normal.y|` everywhere else.
    fn fill_solids(&mut self) {
        if !self.undersides.is_empty() && !self.uppers.is_empty() {
            self.uppers.sort_by(f32::total_cmp);
            for &base in &self.undersides {
                if let Some(&y) = self.uppers.iter().find(|u| **u > base + TOUCHING) {
                    self.surfaces.push(Surface { y, base, flat_enough: false });
                }
            }
        }
        // Scratch, and there are a great many columns. Drop the allocations
        // rather than carry two empty `Vec`s per column for the rest of a bake.
        self.undersides = Vec::new();
        self.uppers = Vec::new();
    }

    /// Fold the stack into one entry per solid, lowest first.
    ///
    /// Anything that touches or overlaps what is below it is part of the same
    /// solid: a wall standing on a floor, the six faces of a box, the two
    /// triangles of one floor meeting along their seam. **Whatever reaches
    /// highest decides whether the result can be stood on**, which is what turns
    /// a floor with a wall on it into a wall.
    fn merge(&mut self) {
        if self.surfaces.len() < 2 {
            return;
        }
        // By foot, then by top — so the piece that starts lowest is always the
        // one the sweep is currently extending.
        self.surfaces.sort_by(|a, b| a.base.total_cmp(&b.base).then(a.y.total_cmp(&b.y)));
        let mut out: Vec<Surface> = Vec::with_capacity(self.surfaces.len());
        for s in std::mem::take(&mut self.surfaces) {
            match out.last_mut() {
                Some(cur) if s.base <= cur.y + TOUCHING => {
                    if s.y > cur.y + TOUCHING {
                        cur.y = s.y;
                        cur.flat_enough = s.flat_enough;
                    } else if s.y >= cur.y - TOUCHING {
                        // A tie at the top: the face you could stand on wins,
                        // because it is the face you would be standing on. The
                        // top of a kerb is its top face, not the four vertical
                        // sides that end at the same height.
                        cur.flat_enough |= s.flat_enough;
                    }
                    // Otherwise it ends below the top already recorded, so it is
                    // buried and changes nothing.
                }
                _ => out.push(s),
            }
        }
        self.surfaces = out;
    }
}

/// How close two heights have to be to be the same height. Generous next to f32
/// error on world coordinates, far tighter than any cell size worth baking at.
const TOUCHING: f32 = 1e-4;

/// The level, sampled into a grid of columns.
#[derive(Clone, Debug)]
pub struct Heightfield {
    /// World position of the corner of column (0, 0).
    pub origin: [f32; 3],
    pub cell_size: f32,
    pub width: usize,
    pub depth: usize,
    columns: Vec<Column>,
}

impl Heightfield {
    pub fn column(&self, x: usize, z: usize) -> Option<&Column> {
        if x >= self.width || z >= self.depth {
            return None;
        }
        self.columns.get(z * self.width + x)
    }

    /// The world position of a column's centre, at a given height.
    pub fn centre_of(&self, x: usize, z: usize, y: f32) -> [f32; 3] {
        [
            self.origin[0] + (x as f32 + 0.5) * self.cell_size,
            y,
            self.origin[2] + (z as f32 + 0.5) * self.cell_size,
        ]
    }

    /// How many columns hold at least one surface you could stand on. Cheap,
    /// and the number worth printing after a bake: zero means the bake found no
    /// floor, which is a very different problem from a bad path.
    pub fn standable_columns(&self, agent_height: f32) -> usize {
        self.columns.iter().filter(|c| !c.walkable(agent_height).is_empty()).count()
    }

    /// Sample `tris` into columns.
    ///
    /// Returns `None` for geometry that cannot be sampled — no triangles, or a
    /// cell size of zero — rather than a heightfield of nothing that would read
    /// downstream as "this level has no floor".
    pub fn build(tris: &[Tri], settings: &NavSettings) -> Option<Heightfield> {
        if tris.is_empty() || settings.cell_size <= 0.0 {
            return None;
        }
        let cell = settings.cell_size;
        let walkable_dot = settings.walkable_dot();

        let (mut lo, mut hi) = tris[0].bounds();
        for t in &tris[1..] {
            let (tlo, thi) = t.bounds();
            for i in 0..3 {
                lo[i] = lo[i].min(tlo[i]);
                hi[i] = hi[i].max(thi[i]);
            }
        }
        // One column of margin, so geometry sitting exactly on the boundary is
        // inside the grid rather than on its edge.
        let origin = [lo[0] - cell, lo[1], lo[2] - cell];
        let width = (((hi[0] - lo[0]) / cell).ceil() as usize) + 3;
        let depth = (((hi[2] - lo[2]) / cell).ceil() as usize) + 3;
        let mut columns = vec![Column::default(); width * depth];

        for t in tris {
            let n = t.normal();
            let flat_enough = n[1].abs() >= walkable_dot;
            let (tlo, thi) = t.bounds();
            // Only the columns this triangle's footprint touches.
            let x0 = (((tlo[0] - origin[0]) / cell).floor() as isize).max(0) as usize;
            let x1 = (((thi[0] - origin[0]) / cell).ceil() as isize).max(0) as usize;
            let z0 = (((tlo[2] - origin[2]) / cell).floor() as isize).max(0) as usize;
            let z1 = (((thi[2] - origin[2]) / cell).ceil() as isize).max(0) as usize;
            for z in z0..=z1.min(depth.saturating_sub(1)) {
                for x in x0..=x1.min(width.saturating_sub(1)) {
                    let sq = [
                        [origin[0] + x as f32 * cell, origin[2] + z as f32 * cell],
                        [origin[0] + (x + 1) as f32 * cell, origin[2] + (z + 1) as f32 * cell],
                    ];
                    let cx = sq[0][0] + cell * 0.5;
                    let cz = sq[0][1] + cell * 0.5;
                    if !overlaps(t, sq, cell) {
                        continue;
                    }
                    let col = &mut columns[z * width + x];
                    if flat_enough {
                        // Exact where the centre is on the triangle — that is
                        // what keeps a ramp a ramp. Where it is not, the column
                        // is a fringe one the triangle only clips, so read the
                        // height off its plane and hold it to the triangle's own
                        // range so a sliver cannot extrapolate wildly.
                        let y = height_at(t, cx, cz)
                            .unwrap_or_else(|| plane_y(t, &n, cx, cz).clamp(tlo[1], thi[1]));
                        col.surfaces.push(Surface::ground(y, true));
                        // Which way it looks, for `fill_solids`. Never used to
                        // decide walkability — that stays winding-blind.
                        if n[1] >= 0.0 {
                            col.uppers.push(y);
                        } else {
                            col.undersides.push(y);
                        }
                    } else {
                        let (base, y) = solid_span(t, &n, sq, tlo, thi);
                        col.surfaces.push(Surface { y, base, flat_enough: false });
                    }
                }
            }
        }

        for col in &mut columns {
            col.fill_solids();
            col.merge();
        }

        Some(Heightfield { origin, cell_size: cell, width, depth, columns })
    }
}

/// Where a triangle sits over one point, or `None` if the point is outside it.
///
/// Barycentric, computed in the XZ plane — a vertical triangle projects to a
/// line of zero area there and is rejected, which is correct: you cannot stand
/// on a wall, and it has no single height over a point anyway.
fn height_at(t: &Tri, x: f32, z: f32) -> Option<f32> {
    let (ax, az) = (t.a[0], t.a[2]);
    let (bx, bz) = (t.b[0], t.b[2]);
    let (cx, cz) = (t.c[0], t.c[2]);
    let det = (bz - cz) * (ax - cx) + (cx - bx) * (az - cz);
    if det.abs() <= f32::EPSILON {
        return None;
    }
    let l1 = ((bz - cz) * (x - cx) + (cx - bx) * (z - cz)) / det;
    let l2 = ((cz - az) * (x - cx) + (ax - cx) * (z - cz)) / det;
    let l3 = 1.0 - l1 - l2;
    // A hair of tolerance so a point exactly on a shared edge lands on one of
    // the two triangles rather than falling between them.
    const E: f32 = -1e-5;
    if l1 < E || l2 < E || l3 < E {
        return None;
    }
    Some(l1 * t.a[1] + l2 * t.b[1] + l3 * t.c[1])
}

/// The height of the triangle's **plane** over a point, whether or not the point
/// is inside the triangle. Vertical planes have no answer, and say so with the
/// triangle's own lowest corner — the caller clamps, so it never escapes.
fn plane_y(t: &Tri, n: &[f32; 3], x: f32, z: f32) -> f32 {
    if n[1].abs() <= 1e-6 {
        return t.a[1];
    }
    t.a[1] - (n[0] * (x - t.a[0]) + n[2] * (z - t.a[2])) / n[1]
}

/// How much solid a face too steep to stand on puts in one column, foot to top.
///
/// A **vertical** face is the whole point of this: it has no height over a
/// point, so it occupies its full height everywhere its footprint lands. That is
/// what makes a wall a wall.
///
/// A face that is steep but not vertical — a cliff, an overhang, a slope past
/// the limit — does have a height, so its span is read off the plane across the
/// part of the column its footprint can actually reach. Taking the whole
/// triangle's height range instead would let one tall cliff triangle black out
/// the ground at its foot.
fn solid_span(
    t: &Tri,
    n: &[f32; 3],
    sq: [[f32; 2]; 2],
    tlo: [f32; 3],
    thi: [f32; 3],
) -> (f32, f32) {
    if n[1].abs() <= 1e-6 {
        return (tlo[1], thi[1]);
    }
    let x0 = sq[0][0].max(tlo[0]);
    let x1 = sq[1][0].min(thi[0]).max(x0);
    let z0 = sq[0][1].max(tlo[2]);
    let z1 = sq[1][1].min(thi[2]).max(z0);
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    // The plane is linear, so its extremes over a rectangle are at its corners.
    for (x, z) in [(x0, z0), (x1, z0), (x0, z1), (x1, z1)] {
        let y = plane_y(t, n, x, z);
        lo = lo.min(y);
        hi = hi.max(y);
    }
    (lo.clamp(tlo[1], thi[1]), hi.clamp(tlo[1], thi[1]))
}

/// Does a triangle's footprint touch a column's square, in plan?
///
/// Separating-axis, over the square's two axes and the triangle's three edge
/// normals. It answers for a triangle of **zero area** — the projection of a
/// vertical wall — which is the case a point-in-triangle test cannot, and the
/// reason this exists.
///
/// Touching exactly counts as overlapping, by a hair of `cell`. A wall built
/// flush with a grid line would otherwise fall between two columns and block
/// neither; claiming both is a half-cell of caution in the direction that keeps
/// characters out of walls.
fn overlaps(t: &Tri, sq: [[f32; 2]; 2], cell: f32) -> bool {
    let e = cell * 1e-3;
    let p = [[t.a[0], t.a[2]], [t.b[0], t.b[2]], [t.c[0], t.c[2]]];
    for i in 0..2 {
        let lo = p[0][i].min(p[1][i]).min(p[2][i]);
        let hi = p[0][i].max(p[1][i]).max(p[2][i]);
        if hi < sq[0][i] - e || lo > sq[1][i] + e {
            return false;
        }
    }
    let corners = [
        [sq[0][0], sq[0][1]],
        [sq[1][0], sq[0][1]],
        [sq[1][0], sq[1][1]],
        [sq[0][0], sq[1][1]],
    ];
    for k in 0..3 {
        let (a, b) = (p[k], p[(k + 1) % 3]);
        let axis = [-(b[1] - a[1]), b[0] - a[0]];
        let len = (axis[0] * axis[0] + axis[1] * axis[1]).sqrt();
        if len <= f32::EPSILON {
            continue; // A repeated vertex: no edge, no axis.
        }
        let axis = [axis[0] / len, axis[1] / len];
        let project = |q: [f32; 2]| axis[0] * q[0] + axis[1] * q[1];
        let (mut tlo, mut thi) = (f32::INFINITY, f32::NEG_INFINITY);
        for q in p {
            let d = project(q);
            tlo = tlo.min(d);
            thi = thi.max(d);
        }
        let (mut blo, mut bhi) = (f32::INFINITY, f32::NEG_INFINITY);
        for q in corners {
            let d = project(q);
            blo = blo.min(d);
            bhi = bhi.max(d);
        }
        if thi < blo - e || tlo > bhi + e {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 4×4 floor of two triangles.
    fn floor(y: f32) -> Vec<Tri> {
        vec![
            Tri::new([0.0, y, 0.0], [4.0, y, 0.0], [0.0, y, 4.0]),
            Tri::new([4.0, y, 0.0], [4.0, y, 4.0], [0.0, y, 4.0]),
        ]
    }

    #[test]
    fn a_floor_samples_into_columns_at_its_own_height() {
        let s = NavSettings { cell_size: 0.5, ..Default::default() };
        let hf = Heightfield::build(&floor(2.0), &s).unwrap();
        let mid = hf.column(hf.width / 2, hf.depth / 2).unwrap();
        assert_eq!(mid.surfaces.len(), 1, "the seam must not read as two floors");
        assert!((mid.surfaces[0].y - 2.0).abs() < 1e-4);
        assert!(mid.surfaces[0].flat_enough);
        assert!(hf.standable_columns(s.agent_height) >= 60, "a 4x4 floor at 0.5 is ~64 columns");
    }

    /// The case the whole stack-of-surfaces design exists for: under a bridge is
    /// a different place from on it.
    #[test]
    fn a_floor_under_a_bridge_keeps_both_surfaces() {
        let s = NavSettings { cell_size: 0.5, agent_height: 1.8, ..Default::default() };
        let mut tris = floor(0.0);
        tris.extend(floor(4.0)); // plenty of headroom below
        let hf = Heightfield::build(&tris, &s).unwrap();
        let mid = hf.column(hf.width / 2, hf.depth / 2).unwrap();
        assert_eq!(mid.surfaces.len(), 2);
        // Both are standable: 4 m of clearance under, open sky over.
        assert_eq!(mid.walkable(s.agent_height).len(), 2);
    }

    /// Headroom is the point of `agent_height`, and a low ceiling must remove
    /// the floor under it rather than merely be noted.
    #[test]
    fn a_ceiling_too_low_to_stand_under_removes_the_floor_below_it() {
        let s = NavSettings { cell_size: 0.5, agent_height: 1.8, ..Default::default() };
        let mut tris = floor(0.0);
        tris.extend(floor(1.0)); // only a metre of headroom
        let hf = Heightfield::build(&tris, &s).unwrap();
        let mid = hf.column(hf.width / 2, hf.depth / 2).unwrap();
        assert_eq!(mid.surfaces.len(), 2);
        let standable = mid.walkable(s.agent_height);
        assert_eq!(standable.len(), 1, "only the top one is standable");
        assert!((standable[0].y - 1.0).abs() < 1e-4);
    }

    /// An axis-aligned box, all six faces — how the modelling tool, the mesh
    /// importer and the box primitive all hand a wall to the baker.
    fn boxy(lo: [f32; 3], hi: [f32; 3]) -> Vec<Tri> {
        let v = |i: usize| {
            [
                if i & 1 == 0 { lo[0] } else { hi[0] },
                if i & 2 == 0 { lo[1] } else { hi[1] },
                if i & 4 == 0 { lo[2] } else { hi[2] },
            ]
        };
        // Wound outward — which matters only for the two horizontal faces, and
        // only so `fill_solids` can tell a box from a room.
        const QUADS: [[usize; 4]; 6] = [
            [0, 1, 3, 2], // -z
            [4, 5, 7, 6], // +z
            [0, 1, 5, 4], // -y, looking down
            [2, 6, 7, 3], // +y, looking up
            [0, 2, 6, 4], // -x
            [1, 3, 7, 5], // +x
        ];
        let mut out = Vec::new();
        for q in QUADS {
            out.push(Tri::new(v(q[0]), v(q[1]), v(q[2])));
            out.push(Tri::new(v(q[0]), v(q[2]), v(q[3])));
        }
        out
    }

    /// **The bug this rewrite exists for.** A wall drawn thinner than a column
    /// has no column centre inside it, so sampling saw nothing at all and the
    /// ground ran straight through it.
    #[test]
    fn a_wall_thinner_than_a_column_still_blocks_the_ground_under_it() {
        let s = NavSettings { cell_size: 0.5, agent_height: 1.8, ..Default::default() };
        let mut tris = floor(0.0);
        // 8 cm thick, and deliberately nowhere near a column centre.
        tris.extend(boxy([1.93, 0.0, 0.0], [2.01, 3.0, 4.0]));
        let hf = Heightfield::build(&tris, &s).unwrap();

        let x = ((2.0 - hf.origin[0]) / hf.cell_size) as usize;
        let z = hf.depth / 2;
        let col = hf.column(x, z).unwrap();
        assert!(
            col.walkable(s.agent_height).iter().all(|w| w.y > 2.0),
            "the ground under a wall is not ground: {:?}",
            col.surfaces
        );
        assert!(
            col.surfaces[0].base <= 0.0 + 1e-3 && col.surfaces[0].y >= 3.0 - 1e-3,
            "the wall should fill the column foot to top: {:?}",
            col.surfaces[0]
        );
    }

    /// The same wall, walked across the grid. Wherever it lands relative to the
    /// column boundaries it has to block every column it crosses — a wall that
    /// blocks at some offsets and not others is the "it noticed part of it"
    /// report.
    #[test]
    fn a_wall_blocks_at_every_offset_against_the_grid() {
        let s = NavSettings { cell_size: 0.5, agent_height: 1.8, ..Default::default() };
        for step in 0..8 {
            let x0 = 1.5 + step as f32 * 0.0625;
            let mut tris = floor(0.0);
            tris.extend(boxy([x0, 0.0, 0.0], [x0 + 0.08, 3.0, 4.0]));
            let hf = Heightfield::build(&tris, &s).unwrap();
            let z = hf.depth / 2;
            // Where the room's own floor survives, along one row.
            let ground: Vec<bool> = (0..hf.width)
                .map(|x| {
                    hf.column(x, z)
                        .is_some_and(|c| c.walkable(s.agent_height).iter().any(|w| w.y < 1.0))
                })
                .collect();
            let first = ground.iter().position(|g| *g).unwrap();
            let last = ground.iter().rposition(|g| *g).unwrap();
            assert!(
                ground[first..last].iter().any(|g| !*g),
                "the floor ran straight through the wall at x0 = {x0}: {ground:?}"
            );
            // …and the room is still a room on both sides of it.
            assert!(last - first > 4, "blocking must not eat the floor: x0 = {x0}");
        }
    }

    /// The other half of the same trade: ground thinner than a column used to
    /// fall between centres too. A catwalk is somewhere to walk.
    #[test]
    fn ground_thinner_than_a_column_is_not_lost() {
        let s = NavSettings { cell_size: 0.5, ..Default::default() };
        // A 20 cm strip, an arbitrary distance off the grid.
        let plank = vec![
            Tri::new([0.13, 1.0, 0.0], [0.33, 1.0, 0.0], [0.13, 1.0, 4.0]),
            Tri::new([0.33, 1.0, 0.0], [0.33, 1.0, 4.0], [0.13, 1.0, 4.0]),
        ];
        let hf = Heightfield::build(&plank, &s).unwrap();
        assert!(
            hf.standable_columns(s.agent_height) >= 8,
            "a 4 m plank should hold up ~8 columns, got {}",
            hf.standable_columns(s.agent_height)
        );
    }

    /// A kerb's four vertical sides end at exactly the height of its top face.
    /// If the sides win that tie, every step in the level becomes unwalkable.
    #[test]
    fn the_top_of_a_kerb_is_still_somewhere_to_stand() {
        let s = NavSettings { cell_size: 0.1, agent_height: 1.8, ..Default::default() };
        let hf = Heightfield::build(&boxy([0.0, 0.0, 0.0], [2.0, 0.2, 2.0]), &s).unwrap();
        let top = hf.column(hf.width / 2, hf.depth / 2).unwrap().walkable(s.agent_height);
        assert_eq!(top.len(), 1, "the top of the kerb");
        assert!((top[0].y - 0.2).abs() < 1e-3, "{top:?}");
    }

    /// A wall standing on a floor is one solid, so the floor beneath it is gone
    /// rather than merely low on headroom — and the clearance test has to
    /// measure to the wall's **foot**, not its top.
    #[test]
    fn a_wall_and_the_floor_it_stands_on_merge_into_one_solid() {
        let s = NavSettings { cell_size: 0.5, agent_height: 1.8, ..Default::default() };
        let mut tris = floor(0.0);
        tris.extend(boxy([1.0, 0.0, 0.0], [3.0, 3.0, 4.0]));
        let hf = Heightfield::build(&tris, &s).unwrap();
        let col = hf.column(hf.width / 2, hf.depth / 2).unwrap();
        assert_eq!(col.surfaces.len(), 1, "floor and wall are one solid: {:?}", col.surfaces);
        assert!(col.surfaces[0].flat_enough, "you can stand on top of a wall");
        assert!((col.surfaces[0].y - 3.0).abs() < 1e-3);
    }

    /// A solid thicker than two columns has columns in the middle of it with no
    /// vertical face anywhere near them — only its underside at the bottom and
    /// its top at the top. Nothing but which way those two look says the gap
    /// between them is stone rather than a room.
    #[test]
    fn the_inside_of_a_thick_pillar_is_not_a_room() {
        let s = NavSettings { cell_size: 0.15, agent_height: 1.8, ..Default::default() };
        let mut tris = floor(0.0);
        tris.extend(boxy([1.0, 0.0, 1.0], [3.0, 3.0, 3.0]));
        let hf = Heightfield::build(&tris, &s).unwrap();
        let x = ((2.0 - hf.origin[0]) / hf.cell_size) as usize;
        let z = ((2.0 - hf.origin[2]) / hf.cell_size) as usize;
        let col = hf.column(x, z).unwrap();
        assert_eq!(col.surfaces.len(), 1, "solid, foot to top: {:?}", col.surfaces);
        assert!((col.surfaces[0].y - 3.0).abs() < 1e-3, "{:?}", col.surfaces[0]);
        assert!(
            col.walkable(s.agent_height).iter().all(|w| w.y > 2.0),
            "there is no floor inside a pillar"
        );
    }

    /// …and the mirror image, which is the whole risk of reading winding at all:
    /// a room is a floor that looks up with a ceiling that looks down, and its
    /// floor has to survive.
    #[test]
    fn the_inside_of_a_room_is_still_a_room() {
        let s = NavSettings { cell_size: 0.5, agent_height: 1.8, ..Default::default() };
        // Floor looking up, ceiling looking down — a room, not a slab.
        let mut tris = vec![
            Tri::new([0.0, 0.0, 0.0], [0.0, 0.0, 4.0], [4.0, 0.0, 0.0]),
            Tri::new([4.0, 0.0, 0.0], [0.0, 0.0, 4.0], [4.0, 0.0, 4.0]),
        ];
        tris.extend([
            Tri::new([0.0, 3.0, 0.0], [4.0, 3.0, 0.0], [0.0, 3.0, 4.0]),
            Tri::new([4.0, 3.0, 0.0], [4.0, 3.0, 4.0], [0.0, 3.0, 4.0]),
        ]);
        let hf = Heightfield::build(&tris, &s).unwrap();
        let col = hf.column(hf.width / 2, hf.depth / 2).unwrap();
        assert_eq!(col.surfaces.len(), 2, "floor and ceiling: {:?}", col.surfaces);
        let standable = col.walkable(s.agent_height);
        assert!(
            standable.iter().any(|w| w.y.abs() < 1e-3),
            "the floor of a room is somewhere to stand: {standable:?}"
        );
    }

    #[test]
    fn a_wall_contributes_no_walkable_surface() {
        let s = NavSettings { cell_size: 0.5, ..Default::default() };
        let wall = vec![
            Tri::new([0.0, 0.0, 0.0], [0.0, 3.0, 0.0], [4.0, 0.0, 0.0]),
            Tri::new([4.0, 0.0, 0.0], [0.0, 3.0, 0.0], [4.0, 3.0, 0.0]),
        ];
        let hf = Heightfield::build(&wall, &s).unwrap();
        assert_eq!(hf.standable_columns(s.agent_height), 0);
    }

    /// Nothing to bake is not the same as a level with no floor, and the caller
    /// has to be able to tell them apart.
    #[test]
    fn nothing_to_bake_says_so_rather_than_returning_an_empty_field() {
        let s = NavSettings::default();
        assert!(Heightfield::build(&[], &s).is_none());
        let bad = NavSettings { cell_size: 0.0, ..Default::default() };
        assert!(Heightfield::build(&floor(0.0), &bad).is_none());
    }

    #[test]
    fn a_point_outside_a_triangle_has_no_height_on_it() {
        let t = Tri::new([0.0, 1.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 1.0]);
        assert!(height_at(&t, 0.1, 0.1).is_some());
        assert!(height_at(&t, 0.9, 0.9).is_none(), "outside the hypotenuse");
        // A vertical triangle has no height over a point — it is a line in plan.
        let v = Tri::new([0.0, 0.0, 0.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]);
        assert!(height_at(&v, 0.5, 0.0).is_none());
    }

    /// A ramp is the case where sampling at centres has to give the height at
    /// that centre, not at a vertex.
    #[test]
    fn a_ramp_reports_its_own_height_across_its_length() {
        let s = NavSettings { cell_size: 0.5, max_slope: 45.0, ..Default::default() };
        // Rises 1 over 4 — about 14°, comfortably walkable.
        let ramp = vec![
            Tri::new([0.0, 0.0, 0.0], [4.0, 1.0, 0.0], [0.0, 0.0, 4.0]),
            Tri::new([4.0, 1.0, 0.0], [4.0, 1.0, 4.0], [0.0, 0.0, 4.0]),
        ];
        let hf = Heightfield::build(&ramp, &s).unwrap();
        let low = hf.column(2, hf.depth / 2).unwrap().surfaces[0].y;
        let high = hf.column(hf.width - 3, hf.depth / 2).unwrap().surfaces[0].y;
        assert!(high > low + 0.5, "the ramp must climb: {low} → {high}");
        assert!(hf.column(2, hf.depth / 2).unwrap().surfaces[0].flat_enough);
    }
}
