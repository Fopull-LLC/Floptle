//! Where a tilemap's cells are: square, isometric or hexagonal.
//!
//! One place answers every question the grid's shape changes — where a cell's
//! middle is, what outline it has, which cell a point is in, how far the map
//! reaches, and the order cells draw in — so the mesh, the collider, the
//! painting tools and the importer cannot disagree about it.
//!
//! Positions are in the tilemap's local frame, centred on the node, x right and
//! y up. Row 0 is the top row, as everywhere else.

use serde::{Deserialize, Serialize};

/// The shape of a tilemap's cells. A property of the art, so it lives on the
/// tileset: an isometric tileset is drawn as diamonds, and a map painted with
/// it lays its cells out as diamonds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum GridShape {
    /// Square cells in rows and columns.
    #[default]
    Square,
    /// Diamonds, columns running down-right and rows down-left: the usual
    /// isometric grid, as Tiled lays out an isometric map.
    Isometric,
    /// Hexagons with a point at the top, odd rows pushed half a cell right.
    HexPointy,
    /// Hexagons with a flat top, odd columns pushed half a cell down.
    HexFlat,
}

impl GridShape {
    pub const ALL: [GridShape; 4] =
        [GridShape::Square, GridShape::Isometric, GridShape::HexPointy, GridShape::HexFlat];

    pub fn name(self) -> &'static str {
        match self {
            GridShape::Square => "square",
            GridShape::Isometric => "isometric",
            GridShape::HexPointy => "hex (pointy top)",
            GridShape::HexFlat => "hex (flat top)",
        }
    }

    /// A cell's height over its width when the tileset does not say: half for
    /// an isometric diamond, and a regular hexagon's for the two hexes.
    pub fn default_cell_height(self) -> f32 {
        match self {
            GridShape::Square => 1.0,
            GridShape::Isometric => 0.5,
            GridShape::HexPointy => 2.0 / 3f32.sqrt(),
            GridShape::HexFlat => 3f32.sqrt() / 2.0,
        }
    }
}

/// One map's cells, laid out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellLayout {
    pub shape: GridShape,
    pub cols: u32,
    pub rows: u32,
    /// A cell's width, in world units — the map's `tile`.
    pub w: f32,
    /// A cell's height. The same as `w` for a square grid.
    pub h: f32,
    /// What centres the grid on the node.
    off: (f32, f32),
}

impl CellLayout {
    /// `cell_height` is the cell's height over its width; 0 or less takes the
    /// shape's own (see [`GridShape::default_cell_height`]). A square grid is
    /// always square.
    pub fn new(shape: GridShape, cols: u32, rows: u32, tile: f32, cell_height: f32) -> Self {
        let ratio = match shape {
            GridShape::Square => 1.0,
            _ if cell_height > 0.0 => cell_height,
            _ => shape.default_cell_height(),
        };
        let mut l = Self { shape, cols, rows, w: tile, h: tile * ratio, off: (0.0, 0.0) };
        let (lo, hi) = l.raw_bounds();
        l.off = ((lo.0 + hi.0) * 0.5, (lo.1 + hi.1) * 0.5);
        l
    }

    /// The layout of a map painted with `set`.
    pub fn of(set: Option<&crate::TileSet>, cols: u32, rows: u32, tile: f32) -> Self {
        match set {
            Some(s) => Self::new(s.grid, cols, rows, tile, s.cell_height),
            None => Self::new(GridShape::Square, cols, rows, tile, 0.0),
        }
    }

    pub fn is_square(&self) -> bool {
        self.shape == GridShape::Square
    }

    /// A cell's middle before centring.
    fn raw_center(&self, c: i32, r: i32) -> (f32, f32) {
        let (w, h) = (self.w, self.h);
        let (cf, rf) = (c as f32, r as f32);
        match self.shape {
            GridShape::Square => (cf * w + w * 0.5, -(rf * h + h * 0.5)),
            GridShape::Isometric => ((cf - rf) * w * 0.5, -(cf + rf) * h * 0.5),
            GridShape::HexPointy => (cf * w + if r & 1 == 1 { w * 0.5 } else { 0.0 }, -rf * h * 0.75),
            GridShape::HexFlat => (cf * w * 0.75, -(rf * h + if c & 1 == 1 { h * 0.5 } else { 0.0 })),
        }
    }

    fn raw_bounds(&self) -> ((f32, f32), (f32, f32)) {
        let (w, h) = (self.w, self.h);
        let (cols, rows) = (self.cols.max(1) as f32, self.rows.max(1) as f32);
        match self.shape {
            GridShape::Square => ((0.0, -rows * h), (cols * w, 0.0)),
            GridShape::Isometric => (
                (-(rows - 1.0) * w * 0.5 - w * 0.5, -(cols + rows - 2.0) * h * 0.5 - h * 0.5),
                ((cols - 1.0) * w * 0.5 + w * 0.5, h * 0.5),
            ),
            GridShape::HexPointy => {
                let shift = if self.rows > 1 { w * 0.5 } else { 0.0 };
                ((-w * 0.5, -(rows - 1.0) * h * 0.75 - h * 0.5), ((cols - 1.0) * w + w * 0.5 + shift, h * 0.5))
            }
            GridShape::HexFlat => {
                let shift = if self.cols > 1 { h * 0.5 } else { 0.0 };
                ((-w * 0.5, -(rows - 1.0) * h - shift - h * 0.5), ((cols - 1.0) * w * 0.75 + w * 0.5, h * 0.5))
            }
        }
    }

    /// The map's extent, `(min, max)`, in the node's frame.
    pub fn bounds(&self) -> ([f32; 2], [f32; 2]) {
        let (lo, hi) = self.raw_bounds();
        ([lo.0 - self.off.0, lo.1 - self.off.1], [hi.0 - self.off.0, hi.1 - self.off.1])
    }

    /// The middle of cell (`c`, `r`). Any cell, on the map or off it.
    pub fn center(&self, c: i32, r: i32) -> [f32; 2] {
        let (x, y) = self.raw_center(c, r);
        [x - self.off.0, y - self.off.1]
    }

    /// A point in grid space, where cell (`c`, `r`) spans `c..c+1` by
    /// `r..r+1`: a cell's corners, a selection's edges. Exact for square and
    /// isometric grids, whose cells are a linear image of the unit grid; for a
    /// hexagonal one it ignores the half-cell stagger, which is close enough
    /// for the rubber band of a drag and is why a hex grid's lines are drawn
    /// from its cells' outlines instead.
    pub fn grid_point(&self, fx: f32, fy: f32) -> [f32; 2] {
        let (w, h) = (self.w, self.h);
        let (cf, rf) = (fx - 0.5, fy - 0.5);
        let (x, y) = match self.shape {
            GridShape::Square => (cf * w + w * 0.5, -(rf * h + h * 0.5)),
            GridShape::Isometric => ((cf - rf) * w * 0.5, -(cf + rf) * h * 0.5),
            GridShape::HexPointy => (cf * w, -rf * h * 0.75),
            GridShape::HexFlat => (cf * w * 0.75, -rf * h),
        };
        [x - self.off.0, y - self.off.1]
    }

    /// The cell's outline, anticlockwise.
    pub fn outline(&self, c: i32, r: i32) -> Vec<[f32; 2]> {
        let [x, y] = self.center(c, r);
        let (hw, hh) = (self.w * 0.5, self.h * 0.5);
        let rel: &[(f32, f32)] = match self.shape {
            GridShape::Square => &[(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)],
            GridShape::Isometric => &[(-1.0, 0.0), (0.0, -1.0), (1.0, 0.0), (0.0, 1.0)],
            GridShape::HexPointy => {
                &[(1.0, 0.5), (0.0, 1.0), (-1.0, 0.5), (-1.0, -0.5), (0.0, -1.0), (1.0, -0.5)]
            }
            GridShape::HexFlat => {
                &[(1.0, 0.0), (0.5, 1.0), (-0.5, 1.0), (-1.0, 0.0), (-0.5, -1.0), (0.5, -1.0)]
            }
        };
        rel.iter().map(|&(dx, dy)| [x + dx * hw, y + dy * hh]).collect()
    }

    /// A point of the unit square, `(0, 0)` bottom-left, placed in the cell:
    /// how a tile's collider, drawn on a square in the Tiles tab, lands on a
    /// diamond or a hexagon. A diamond is the square turned and squashed, so
    /// the map is exact; a hexagon takes its bounding box.
    pub fn unit_to_cell(&self, c: i32, r: i32, u: f32, v: f32) -> [f32; 2] {
        let [x, y] = self.center(c, r);
        let (hw, hh) = (self.w * 0.5, self.h * 0.5);
        match self.shape {
            // Bottom-left → the diamond's left corner, bottom-right → its
            // bottom, top-left → its top.
            GridShape::Isometric => [x - hw + (u + v) * hw, y + (v - u) * hh],
            _ => [x - hw + u * self.w, y - hh + v * self.h],
        }
    }

    /// The cell a point is in, on the map or off it.
    pub fn cell_at(&self, x: f32, y: f32) -> (i32, i32) {
        let (rx, ry) = (x + self.off.0, y + self.off.1);
        let (w, h) = (self.w.max(1e-6), self.h.max(1e-6));
        match self.shape {
            GridShape::Square => ((rx / w).floor() as i32, (-ry / h).floor() as i32),
            GridShape::Isometric => {
                // In units of the half-diagonals the diamonds are unit squares
                // turned 45°: c - r and c + r, rounded.
                let a = rx / (w * 0.5);
                let b = -ry / (h * 0.5);
                (((a + b) * 0.5).round() as i32, ((b - a) * 0.5).round() as i32)
            }
            GridShape::HexPointy | GridShape::HexFlat => {
                // Near enough by rows and columns, then the neighbour whose
                // outline holds it: a hexagonal cell is the set of points
                // nearer its middle than any other's.
                let (c0, r0) = match self.shape {
                    GridShape::HexPointy => {
                        let r = (-ry / (h * 0.75)).round() as i32;
                        let c = ((rx - if r & 1 == 1 { w * 0.5 } else { 0.0 }) / w).round() as i32;
                        (c, r)
                    }
                    _ => {
                        let c = (rx / (w * 0.75)).round() as i32;
                        let r = ((-ry - if c & 1 == 1 { h * 0.5 } else { 0.0 }) / h).round() as i32;
                        (c, r)
                    }
                };
                let mut best = (c0, r0);
                let mut best_d = f32::MAX;
                for dr in -1..=1 {
                    for dc in -1..=1 {
                        let (c, r) = (c0 + dc, r0 + dr);
                        let [cx, cy] = self.center(c, r);
                        // Distance scaled so a stretched hexagon still picks
                        // by its own shape.
                        let d = ((x - cx) / w).powi(2) + ((y - cy) / h).powi(2);
                        if d < best_d {
                            best_d = d;
                            best = (c, r);
                        }
                    }
                }
                best
            }
        }
    }

    /// The map's cells, front to back: the order a mesh lists them in so a
    /// tile drawn taller than its cell covers the ones behind it. Front is
    /// lower on the screen.
    pub fn draw_order(&self) -> Vec<(u32, u32)> {
        let mut cells: Vec<(u32, u32)> =
            (0..self.rows).flat_map(|r| (0..self.cols).map(move |c| (c, r))).collect();
        if !self.is_square() {
            cells.sort_by(|a, b| {
                let ya = self.center(a.0 as i32, a.1 as i32)[1];
                let yb = self.center(b.0 as i32, b.1 as i32)[1];
                ya.total_cmp(&yb)
            });
        }
        cells
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inside(poly: &[[f32; 2]], p: [f32; 2]) -> bool {
        let mut sign = 0.0f32;
        for i in 0..poly.len() {
            let (a, b) = (poly[i], poly[(i + 1) % poly.len()]);
            let c = (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0]);
            if c.abs() < 1e-6 {
                continue;
            }
            if sign == 0.0 {
                sign = c.signum();
            } else if c.signum() != sign {
                return false;
            }
        }
        true
    }

    /// Every shape: a cell's middle picks that cell, a point just inside each
    /// corner of its outline does too, and the outline is anticlockwise.
    #[test]
    fn a_point_in_a_cell_picks_that_cell() {
        for shape in GridShape::ALL {
            let l = CellLayout::new(shape, 7, 5, 2.0, 0.0);
            for r in -1..6 {
                for c in -1..8 {
                    let [x, y] = l.center(c, r);
                    assert_eq!(l.cell_at(x, y), (c, r), "{shape:?} middle of {c},{r}");
                    let o = l.outline(c, r);
                    let area: f32 = (0..o.len())
                        .map(|i| o[i][0] * o[(i + 1) % o.len()][1] - o[(i + 1) % o.len()][0] * o[i][1])
                        .sum();
                    assert!(area > 0.0, "{shape:?}: outline must be anticlockwise");
                    for p in &o {
                        let q = [x + (p[0] - x) * 0.9, y + (p[1] - y) * 0.9];
                        assert!(inside(&o, q));
                        assert_eq!(l.cell_at(q[0], q[1]), (c, r), "{shape:?} corner of {c},{r}");
                    }
                }
            }
        }
    }

    /// The square layout is the one every tilemap has always had: the same
    /// middles the mesh and the colliders compute.
    #[test]
    fn square_cells_sit_where_they_always_did() {
        let l = CellLayout::new(GridShape::Square, 4, 2, 1.0, 0.0);
        // Column 0, row 0 is the top-left square of a 4×2 map centred on 0.
        assert_eq!(l.center(0, 0), [-1.5, 0.5]);
        assert_eq!(l.center(3, 1), [1.5, -0.5]);
        assert_eq!(l.bounds(), ([-2.0, -1.0], [2.0, 1.0]));
    }

    /// The map's bounds hold every cell's outline, centred on the node.
    #[test]
    fn the_bounds_hold_every_cell() {
        for shape in GridShape::ALL {
            let l = CellLayout::new(shape, 6, 4, 1.0, 0.0);
            let (lo, hi) = l.bounds();
            assert!((lo[0] + hi[0]).abs() < 1e-4 && (lo[1] + hi[1]).abs() < 1e-4, "{shape:?} is not centred");
            for (c, r) in l.draw_order() {
                for p in l.outline(c as i32, r as i32) {
                    assert!(p[0] >= lo[0] - 1e-4 && p[0] <= hi[0] + 1e-4, "{shape:?} {c},{r} x {}", p[0]);
                    assert!(p[1] >= lo[1] - 1e-4 && p[1] <= hi[1] + 1e-4, "{shape:?} {c},{r} y {}", p[1]);
                }
            }
        }
    }

    /// A square grid's corners are where they always were, and an isometric
    /// cell's grid-space corners are its diamond's.
    #[test]
    fn grid_points_are_cell_corners() {
        let sq = CellLayout::new(GridShape::Square, 4, 2, 1.0, 0.0);
        assert_eq!(sq.grid_point(0.0, 0.0), [-2.0, 1.0]);
        assert_eq!(sq.grid_point(4.0, 2.0), [2.0, -1.0]);
        let iso = CellLayout::new(GridShape::Isometric, 3, 3, 2.0, 0.0);
        let o = iso.outline(1, 2);
        assert_eq!(iso.grid_point(1.0, 2.0), o[3], "top corner");
        assert_eq!(iso.grid_point(2.0, 3.0), o[1], "bottom corner");
    }

    /// The unit square lands on the diamond's corners exactly.
    #[test]
    fn a_unit_square_lands_on_the_diamond() {
        let l = CellLayout::new(GridShape::Isometric, 3, 3, 2.0, 0.0);
        let o = l.outline(1, 1);
        assert_eq!(l.unit_to_cell(1, 1, 0.0, 0.0), o[0], "bottom-left → left corner");
        assert_eq!(l.unit_to_cell(1, 1, 1.0, 0.0), o[1], "bottom-right → bottom corner");
        assert_eq!(l.unit_to_cell(1, 1, 1.0, 1.0), o[2], "top-right → right corner");
        assert_eq!(l.unit_to_cell(1, 1, 0.0, 1.0), o[3], "top-left → top corner");
    }

    /// Front to back: an isometric cell lower on the screen comes first.
    #[test]
    fn isometric_cells_draw_front_to_back() {
        let l = CellLayout::new(GridShape::Isometric, 3, 3, 1.0, 0.0);
        let order = l.draw_order();
        assert_eq!(order[0], (2, 2), "the bottom corner is the front");
        assert_eq!(*order.last().unwrap(), (0, 0), "the top corner is the back");
    }
}
