//! The heightfield — the level, sampled into columns.
//!
//! Every triangle is dropped into the square columns its footprint covers, and
//! each column keeps a sorted stack of the surfaces it hit. That stack is what
//! makes a floor under a bridge a different place from the bridge: two surfaces
//! in one column, each with its own headroom.
//!
//! Sampling at column centres rather than clipping each triangle to each column
//! is the simplification the whole baker rests on. It is exact for the case that
//! matters — large floors and ramps, whose surface over a column centre is
//! genuinely where you stand — and it is wrong in one way worth knowing: a
//! surface narrower than a column can fall between centres and go unrecorded.
//! That is the same trade as picking a cell size at all, and the fix when it
//! bites is a smaller cell.

use crate::{NavSettings, Tri};

/// One surface in a column: how high it is, and whether you could stand on it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Surface {
    pub y: f32,
    /// Not too steep. Says nothing about headroom — [`Column::walkable`] adds
    /// that, because clearance depends on what is above and this does not know
    /// yet.
    pub flat_enough: bool,
}

/// The surfaces in one column, lowest first.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Column {
    pub surfaces: Vec<Surface>,
}

impl Column {
    /// Surfaces you could actually stand on: flat enough, **and** with at least
    /// `agent_height` of nothing above them.
    ///
    /// The ceiling is the next surface up, whatever it is — a walkable floor
    /// above you is still a ceiling, which is why this asks the stack rather
    /// than only the unwalkable surfaces.
    pub fn walkable(&self, agent_height: f32) -> Vec<Surface> {
        self.surfaces
            .iter()
            .enumerate()
            .filter(|(i, s)| {
                if !s.flat_enough {
                    return false;
                }
                match self.surfaces.get(i + 1) {
                    Some(above) => above.y - s.y >= agent_height,
                    // Nothing above it at all: open sky.
                    None => true,
                }
            })
            .map(|(_, s)| *s)
            .collect()
    }
}

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
                    let cx = origin[0] + (x as f32 + 0.5) * cell;
                    let cz = origin[2] + (z as f32 + 0.5) * cell;
                    if let Some(y) = height_at(t, cx, cz) {
                        columns[z * width + x].surfaces.push(Surface { y, flat_enough });
                    }
                }
            }
        }

        for col in &mut columns {
            col.surfaces.sort_by(|a, b| a.y.total_cmp(&b.y));
            // Two surfaces at the same height are one surface — a floor made of
            // two triangles meeting along an edge hits both in the same column,
            // and left alone that pair reads as a floor with no headroom above
            // it. Unwalkable wins a tie: a wall meeting a floor must not be
            // flattened into walkable ground.
            col.surfaces.dedup_by(|b, a| {
                if (b.y - a.y).abs() <= f32::EPSILON * 8.0 {
                    a.flat_enough &= b.flat_enough;
                    true
                } else {
                    false
                }
            });
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
