//! The edit operations every tile tool is made of, over a plain grid.
//!
//! ## Why the tools live here and not in the editor panel
//!
//! A brush, a rectangle, a flood fill and a stamp are pure functions of (grid,
//! coordinates, what to write). Put them in the panel and the only way to check
//! that a flood fill stops at a diagonal, or that a rotated stamp lands where its
//! preview showed, is to run the editor and look. Put them here and each one is a
//! test that runs in the gate.
//!
//! Everything is expressed against [`TileGrid`], a borrowed view over a
//! `Matter::Tilemap`'s own `(cols, rows, data)`. There is no second copy of the
//! map: an edit writes straight into the scene's own component, which is what
//! makes tile edits ordinary scene undo rather than a private history the panel
//! has to keep in step.
//!
//! ## Coordinates
//!
//! `(x, y)` is 0-based from the TOP-LEFT, matching `data`'s row-major order and
//! `tm:set` in Lua. Every function takes signed coordinates and treats
//! out-of-range as a no-op rather than wrapping or panicking — a rectangle
//! dragged off the edge of the map should clip, which is what a person means by
//! dragging it off the edge.

use floptle_core::{tile_index, tile_pack, tile_reoriented, tile_xform, TileXform, EMPTY_TILE};

use crate::autotile::{canonical, Autotiler, OFFSETS};
use crate::tileset::TileSet;

/// A borrowed, writable view of one tilemap's grid.
pub struct TileGrid<'a> {
    pub cols: u32,
    pub rows: u32,
    pub data: &'a mut Vec<u32>,
}

impl<'a> TileGrid<'a> {
    /// Wrap a component's fields, sizing `data` to the grid if it is short.
    ///
    /// A short `data` fills with holes rather than refusing: the same choice
    /// `node:setTilemap` makes, so a grid resized by one path is not a different
    /// shape from one resized by another.
    pub fn new(cols: u32, rows: u32, data: &'a mut Vec<u32>) -> Self {
        let want = (cols as usize) * (rows as usize);
        if data.len() != want {
            data.resize(want, EMPTY_TILE);
        }
        Self { cols, rows, data }
    }

    fn index(&self, x: i32, y: i32) -> Option<usize> {
        (x >= 0 && y >= 0 && (x as u32) < self.cols && (y as u32) < self.rows)
            .then(|| (y as usize) * (self.cols as usize) + (x as usize))
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        self.index(x, y).is_some()
    }

    /// The packed square at `(x, y)`, or `None` outside the grid.
    ///
    /// Outside is `None` rather than `EMPTY_TILE` on purpose: autotiling has to
    /// distinguish "off the map" from "a hole in the map", because a shape drawn
    /// to the very edge should not grow an edge tile against the void.
    pub fn get(&self, x: i32, y: i32) -> Option<u32> {
        self.index(x, y).and_then(|i| self.data.get(i).copied())
    }

    /// Write a square. Returns whether anything changed — the panel coalesces a
    /// drag into one undo step and needs to know whether the step is empty.
    pub fn set(&mut self, x: i32, y: i32, packed: u32) -> bool {
        let Some(i) = self.index(x, y) else { return false };
        let Some(slot) = self.data.get_mut(i) else { return false };
        if *slot == packed {
            return false;
        }
        *slot = packed;
        true
    }

    pub fn clear(&mut self, x: i32, y: i32) -> bool {
        self.set(x, y, EMPTY_TILE)
    }

    /// Paint a filled rectangle between two corners, in either order.
    pub fn fill_rect(&mut self, a: (i32, i32), b: (i32, i32), packed: u32) -> bool {
        let (x0, x1) = (a.0.min(b.0), a.0.max(b.0));
        let (y0, y1) = (a.1.min(b.1), a.1.max(b.1));
        let mut hit = false;
        for y in y0..=y1 {
            for x in x0..=x1 {
                hit |= self.set(x, y, packed);
            }
        }
        hit
    }

    /// Paint the OUTLINE of a rectangle between two corners.
    pub fn stroke_rect(&mut self, a: (i32, i32), b: (i32, i32), packed: u32) -> bool {
        let (x0, x1) = (a.0.min(b.0), a.0.max(b.0));
        let (y0, y1) = (a.1.min(b.1), a.1.max(b.1));
        let mut hit = false;
        for x in x0..=x1 {
            hit |= self.set(x, y0, packed);
            hit |= self.set(x, y1, packed);
        }
        for y in y0..=y1 {
            hit |= self.set(x0, y, packed);
            hit |= self.set(x1, y, packed);
        }
        hit
    }

    /// Paint a straight line between two squares (Bresenham), so a drag with the
    /// line tool is a line and a fast mouse does not leave gaps.
    pub fn line(&mut self, a: (i32, i32), b: (i32, i32), packed: u32) -> bool {
        let (mut x, mut y) = a;
        let (dx, dy) = ((b.0 - a.0).abs(), -(b.1 - a.1).abs());
        let (sx, sy) = (if a.0 < b.0 { 1 } else { -1 }, if a.1 < b.1 { 1 } else { -1 });
        let mut err = dx + dy;
        let mut hit = false;
        loop {
            hit |= self.set(x, y, packed);
            if (x, y) == b {
                break;
            }
            let e2 = err * 2;
            if e2 >= dy {
                err += dy;
                x += sx;
            }
            if e2 <= dx {
                err += dx;
                y += sy;
            }
        }
        hit
    }

    /// Flood fill from `(x, y)`, replacing everything connected that matches what
    /// is there now.
    ///
    /// **Four-connected, not eight.** Two squares that touch only at a corner are
    /// not the same region — that is what everyone means by a bucket fill, and
    /// eight-connectivity leaks a fill through a diagonal wall, which is the one
    /// mistake in a bucket tool nobody forgives.
    ///
    /// Matching compares the CELL, not the orientation: filling a region of the
    /// same tile placed at different rotations is one region, because it looks
    /// like one region.
    pub fn flood_fill(&mut self, x: i32, y: i32, packed: u32) -> bool {
        let Some(start) = self.get(x, y) else { return false };
        let target = flood_key(start);
        if flood_key(packed) == target {
            return false; // already what you asked for; a fill would loop for nothing
        }
        let mut stack = vec![(x, y)];
        let mut hit = false;
        // Bounded by the grid: every square is pushed at most once because the
        // first thing done to it is overwrite it out of the target class.
        while let Some((cx, cy)) = stack.pop() {
            if self.get(cx, cy).map(flood_key) != Some(target) {
                continue;
            }
            hit |= self.set(cx, cy, packed);
            stack.extend([(cx + 1, cy), (cx - 1, cy), (cx, cy + 1), (cx, cy - 1)]);
        }
        hit
    }

    /// Replace every square whose cell matches the one at `(x, y)` anywhere in the
    /// grid — the "global replace" a palette swap wants.
    pub fn replace_all(&mut self, x: i32, y: i32, packed: u32) -> bool {
        let Some(start) = self.get(x, y) else { return false };
        let target = flood_key(start);
        let mut hit = false;
        for i in 0..self.data.len() {
            if self.data.get(i).copied().map(flood_key) == Some(target) {
                let slot = &mut self.data[i];
                if *slot != packed {
                    *slot = packed;
                    hit = true;
                }
            }
        }
        hit
    }

    /// Copy a rectangle out as a reusable [`Stamp`].
    pub fn copy_rect(&self, a: (i32, i32), b: (i32, i32)) -> Stamp {
        let (x0, x1) = (a.0.min(b.0), a.0.max(b.0));
        let (y0, y1) = (a.1.min(b.1), a.1.max(b.1));
        let cols = (x1 - x0 + 1).max(0) as u32;
        let rows = (y1 - y0 + 1).max(0) as u32;
        let mut data = Vec::with_capacity((cols * rows) as usize);
        for y in y0..=y1 {
            for x in x0..=x1 {
                data.push(self.get(x, y).unwrap_or(EMPTY_TILE));
            }
        }
        Stamp { cols, rows, data }
    }

    /// Place a stamp with its top-left at `(x, y)`.
    ///
    /// `skip_empty` is the difference between a stamp that paints and one that
    /// replaces: a decal stamped over ground should leave the ground showing
    /// through its holes, while a room stamped into a level should bring its own
    /// empty space with it.
    pub fn stamp(&mut self, x: i32, y: i32, s: &Stamp, skip_empty: bool) -> bool {
        let mut hit = false;
        for (i, &packed) in s.data.iter().enumerate() {
            if skip_empty && packed == EMPTY_TILE {
                continue;
            }
            let (dx, dy) = ((i as u32 % s.cols.max(1)) as i32, (i as u32 / s.cols.max(1)) as i32);
            hit |= self.set(x + dx, y + dy, packed);
        }
        hit
    }

    /// Move a rectangle's contents by `(dx, dy)`, leaving holes behind.
    ///
    /// Reads the whole source before writing any of it, so a move onto overlapping
    /// ground works — the obvious in-place loop smears the region across itself in
    /// whichever direction it happens to walk.
    pub fn move_rect(&mut self, a: (i32, i32), b: (i32, i32), dx: i32, dy: i32) -> bool {
        if dx == 0 && dy == 0 {
            return false;
        }
        let lifted = self.copy_rect(a, b);
        let (x0, y0) = (a.0.min(b.0), a.1.min(b.1));
        let mut hit = self.fill_rect(a, b, EMPTY_TILE);
        hit |= self.stamp(x0 + dx, y0 + dy, &lifted, false);
        hit
    }

    /// Re-orient every square in a rectangle, keeping their cells.
    pub fn reorient_rect(&mut self, a: (i32, i32), b: (i32, i32), xf: TileXform) -> bool {
        let (x0, x1) = (a.0.min(b.0), a.0.max(b.0));
        let (y0, y1) = (a.1.min(b.1), a.1.max(b.1));
        let mut hit = false;
        for y in y0..=y1 {
            for x in x0..=x1 {
                if let Some(p) = self.get(x, y) {
                    hit |= self.set(x, y, tile_reoriented(p, xf));
                }
            }
        }
        hit
    }

    /// The 8-neighbour mask at `(x, y)` for an autotile group: which neighbours
    /// hold a tile of that group, or of a group it joins.
    ///
    /// **Off the map counts as filled.** A shape painted to the edge of the grid
    /// should not grow a coastline against the border — the map ends there, it is
    /// not a hole. This is the one rule people notice immediately when it is
    /// wrong, because every level's outer wall comes out edged.
    pub fn neighbour_mask(&self, x: i32, y: i32, group: u16, set: &TileSet) -> u8 {
        let mut mask = 0u8;
        for (dx, dy, bit) in OFFSETS {
            let (nx, ny) = (x + dx, y + dy);
            let filled = match self.get(nx, ny) {
                None => true, // off the map
                Some(p) => set
                    .group_of(tile_index(p))
                    .is_some_and(|g| set.joins(group, g)),
            };
            if filled {
                mask |= bit;
            }
        }
        mask
    }

    /// Recompute the autotiled squares in a rectangle GROWN BY ONE.
    ///
    /// The one-ring is not an optimisation, it is the correctness condition:
    /// painting a square changes what its neighbours should draw, so a retile that
    /// covered only the painted squares would leave a seam of stale edge tiles
    /// exactly one square wide around every stroke.
    ///
    /// Squares whose group has nothing authored for their neighbourhood are LEFT
    /// ALONE (see [`Autotiler::resolve`]) — a half-drawn group makes holes in what
    /// you paint, never erases what was already there.
    pub fn retile(
        &mut self,
        a: (i32, i32),
        b: (i32, i32),
        set: &TileSet,
        at: &Autotiler,
    ) -> usize {
        let (x0, x1) = (a.0.min(b.0) - 1, a.0.max(b.0) + 1);
        let (y0, y1) = (a.1.min(b.1) - 1, a.1.max(b.1) + 1);
        // Read the masks BEFORE writing anything: a retile that wrote as it went
        // would have later squares mask against tiles the same pass just changed,
        // so the result would depend on scan order.
        let mut writes: Vec<(i32, i32, u32)> = Vec::new();
        for y in y0..=y1 {
            for x in x0..=x1 {
                let Some(p) = self.get(x, y) else { continue };
                let Some(group) = set.group_of(tile_index(p)) else { continue };
                if !at.has_group(group) {
                    continue;
                }
                let mask = self.neighbour_mask(x, y, group, set);
                if let Some(cell) = at.resolve(group, mask) {
                    // Keep the square's own orientation: somebody who turned an
                    // autotiled tile by hand meant it.
                    writes.push((x, y, tile_pack(cell, tile_xform(p))));
                }
            }
        }
        writes.iter().filter(|&&(x, y, p)| self.set(x, y, p)).count()
    }

    /// Resize the grid, keeping what overlaps.
    ///
    /// `(ox, oy)` is where the OLD top-left lands in the new grid, so growing a
    /// map upward is `oy = 1` rather than a separate function. Returns the new
    /// `(cols, rows)` — the caller writes them back onto the component.
    pub fn resized(&self, cols: u32, rows: u32, ox: i32, oy: i32) -> (u32, u32, Vec<u32>) {
        let mut out = vec![EMPTY_TILE; (cols as usize) * (rows as usize)];
        for y in 0..self.rows as i32 {
            for x in 0..self.cols as i32 {
                let (nx, ny) = (x + ox, y + oy);
                if nx < 0 || ny < 0 || nx as u32 >= cols || ny as u32 >= rows {
                    continue;
                }
                if let Some(p) = self.get(x, y) {
                    out[(ny as usize) * (cols as usize) + nx as usize] = p;
                }
            }
        }
        (cols, rows, out)
    }

    /// The tightest rectangle holding every non-empty square, as
    /// `(x0, y0, x1, y1)` inclusive — `None` for an empty map. What "trim to
    /// content" and "frame the map" both need.
    pub fn used_bounds(&self) -> Option<(i32, i32, i32, i32)> {
        let mut b: Option<(i32, i32, i32, i32)> = None;
        for y in 0..self.rows as i32 {
            for x in 0..self.cols as i32 {
                if self.get(x, y) == Some(EMPTY_TILE) || self.get(x, y).is_none() {
                    continue;
                }
                b = Some(match b {
                    None => (x, y, x, y),
                    Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                });
            }
        }
        b
    }
}

/// What a flood fill and a global replace compare squares by: the cell, ignoring
/// orientation. A region of one tile at four rotations is one region.
fn flood_key(packed: u32) -> u32 {
    if packed == EMPTY_TILE { EMPTY_TILE } else { tile_index(packed) }
}

/// A rectangle of squares lifted out of a grid — the multi-tile brush.
///
/// This is what makes "select four tiles in the palette and paint with all four"
/// the same mechanism as copy/paste: both are a `Stamp`, and both can be turned.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stamp {
    pub cols: u32,
    pub rows: u32,
    /// Row-major from the top-left, `cols * rows` long.
    pub data: Vec<u32>,
}

impl Stamp {
    /// A single square.
    pub fn one(packed: u32) -> Self {
        Self { cols: 1, rows: 1, data: vec![packed] }
    }

    /// A rectangle of the sheet, as the palette's rubber-band selection produces:
    /// cells `(px, py)` .. `(px + w, py + h)` of a `sheet_cols`-wide sheet.
    pub fn from_sheet(sheet_cols: u32, px: u32, py: u32, w: u32, h: u32) -> Self {
        Self::from_page(0, sheet_cols, px, py, w, h)
    }

    /// The same rectangle, of a named PAGE of a multi-sheet tileset
    /// (`floptle/0092`). Page 0 is the first sheet, so `from_sheet` is this with
    /// the page every pre-paging index already had.
    pub fn from_page(page: u32, sheet_cols: u32, px: u32, py: u32, w: u32, h: u32) -> Self {
        let sheet_cols = sheet_cols.max(1);
        let (w, h) = (w.max(1), h.max(1));
        let mut data = Vec::with_capacity((w * h) as usize);
        for dy in 0..h {
            for dx in 0..w {
                data.push(floptle_core::tile_cell_of(page, (py + dy) * sheet_cols + px + dx));
            }
        }
        Self { cols: w, rows: h, data }
    }

    pub fn is_empty(&self) -> bool {
        self.cols == 0 || self.rows == 0 || self.data.iter().all(|&p| p == EMPTY_TILE)
    }

    pub fn get(&self, x: u32, y: u32) -> Option<u32> {
        (x < self.cols && y < self.rows).then(|| self.data.get((y * self.cols + x) as usize).copied())?
    }

    /// This stamp turned a quarter-turn clockwise.
    ///
    /// Two things turn: the LAYOUT (a 3×1 horizontal run becomes 1×3 vertical) and
    /// each square's own orientation. Turning only the layout is the bug that
    /// makes a rotated stamp of a pipe corner draw pipes pointing the wrong way —
    /// and it is invisible until the art is directional, which is exactly when a
    /// stamp is worth rotating.
    pub fn rotated_cw(&self) -> Self {
        let (cols, rows) = (self.rows, self.cols);
        let mut data = vec![EMPTY_TILE; (cols as usize) * (rows as usize)];
        for y in 0..self.rows {
            for x in 0..self.cols {
                // A clockwise turn sends (x, y) to (rows - 1 - y, x) — in ROW
                // space, where y counts DOWN the screen.
                let (nx, ny) = (self.rows - 1 - y, x);
                let p = self.get(x, y).unwrap_or(EMPTY_TILE);
                data[(ny * cols + nx) as usize] =
                    tile_reoriented(p, tile_xform(p).rotated_cw());
            }
        }
        Self { cols, rows, data }
    }

    /// This stamp mirrored left-to-right.
    pub fn flipped_x(&self) -> Self {
        let mut data = vec![EMPTY_TILE; self.data.len()];
        for y in 0..self.rows {
            for x in 0..self.cols {
                let p = self.get(x, y).unwrap_or(EMPTY_TILE);
                data[(y * self.cols + (self.cols - 1 - x)) as usize] =
                    tile_reoriented(p, tile_xform(p).flipped_x());
            }
        }
        Self { cols: self.cols, rows: self.rows, data }
    }

    /// This stamp mirrored top-to-bottom.
    pub fn flipped_y(&self) -> Self {
        let mut data = vec![EMPTY_TILE; self.data.len()];
        for y in 0..self.rows {
            for x in 0..self.cols {
                let p = self.get(x, y).unwrap_or(EMPTY_TILE);
                data[((self.rows - 1 - y) * self.cols + x) as usize] =
                    tile_reoriented(p, tile_xform(p).flipped_y());
            }
        }
        Self { cols: self.cols, rows: self.rows, data }
    }

    /// This stamp with every square re-oriented by `xf` — the palette's ⇔ / ⇕ / ↻
    /// buttons applied to a single-square stamp, which is the common case.
    pub fn reoriented(&self, xf: TileXform) -> Self {
        Self {
            cols: self.cols,
            rows: self.rows,
            data: self.data.iter().map(|&p| tile_reoriented(p, xf)).collect(),
        }
    }

    /// Which canonical mask this stamp's tile answers, if it is a single square of
    /// an autotile group — what the palette prints beside a selected tile.
    pub fn single_cell(&self) -> Option<u32> {
        (self.cols == 1 && self.rows == 1)
            .then(|| self.data.first().copied())
            .flatten()
            .filter(|&p| p != EMPTY_TILE)
            .map(tile_index)
    }
}

/// The mask a tile is assigned, canonicalised for its group — the number the
/// palette's 3×3 diagram draws.
pub fn tile_mask(set: &TileSet, cell: u32) -> Option<(u16, u8)> {
    let info = set.info(cell)?;
    let group = info.group?;
    let kind = set.groups.get(group as usize)?.kind;
    Some((group, canonical(kind, info.mask)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autotile::{assign_preset, EDGES, E, N, S, W};
    use crate::tileset::{AutotileGroup, AutotileKind};

    fn grid(cols: u32, rows: u32) -> Vec<u32> {
        vec![EMPTY_TILE; (cols * rows) as usize]
    }

    #[test]
    fn a_short_data_list_is_sized_up_with_holes() {
        let mut d = vec![1, 2];
        let g = TileGrid::new(3, 3, &mut d);
        assert_eq!(g.cols, 3);
        assert_eq!(g.data.len(), 9);
        assert_eq!(g.get(0, 0), Some(1));
        assert_eq!(g.get(2, 2), Some(EMPTY_TILE));
    }

    #[test]
    fn outside_the_grid_reads_as_absent_and_writes_as_nothing() {
        let mut d = grid(2, 2);
        let mut g = TileGrid::new(2, 2, &mut d);
        for (x, y) in [(-1, 0), (0, -1), (2, 0), (0, 2), (99, 99)] {
            assert_eq!(g.get(x, y), None, "({x},{y}) must be absent, not wrapped");
            assert!(!g.set(x, y, 3), "({x},{y}) must not write");
        }
        assert!(g.data.iter().all(|&p| p == EMPTY_TILE), "nothing was written anywhere");
    }

    #[test]
    fn set_reports_whether_it_changed_anything() {
        let mut d = grid(2, 2);
        let mut g = TileGrid::new(2, 2, &mut d);
        assert!(g.set(0, 0, 5), "a real change");
        assert!(!g.set(0, 0, 5), "the same value again is not a change");
        assert!(g.set(0, 0, EMPTY_TILE), "…and clearing it is");
    }

    #[test]
    fn a_rectangle_dragged_off_the_edge_clips_rather_than_wrapping() {
        let mut d = grid(4, 4);
        let mut g = TileGrid::new(4, 4, &mut d);
        g.fill_rect((-2, -2), (1, 1), 7);
        assert_eq!(g.get(0, 0), Some(7));
        assert_eq!(g.get(1, 1), Some(7));
        assert_eq!(g.get(2, 2), Some(EMPTY_TILE));
        assert_eq!(g.get(3, 3), Some(EMPTY_TILE), "no wrap-around to the far corner");
        assert_eq!(g.data.iter().filter(|&&p| p == 7).count(), 4);
    }

    #[test]
    fn corners_may_be_given_in_any_order() {
        let mut a = grid(4, 4);
        let mut b = grid(4, 4);
        TileGrid::new(4, 4, &mut a).fill_rect((3, 1), (1, 2), 2);
        TileGrid::new(4, 4, &mut b).fill_rect((1, 2), (3, 1), 2);
        assert_eq!(a, b);
    }

    #[test]
    fn stroke_rect_is_the_outline_only() {
        let mut d = grid(5, 5);
        let mut g = TileGrid::new(5, 5, &mut d);
        g.stroke_rect((1, 1), (3, 3), 4);
        assert_eq!(g.get(1, 1), Some(4));
        assert_eq!(g.get(2, 1), Some(4));
        assert_eq!(g.get(2, 2), Some(EMPTY_TILE), "the middle is untouched");
        assert_eq!(g.data.iter().filter(|&&p| p == 4).count(), 8);
    }

    #[test]
    fn a_line_is_continuous() {
        let mut d = grid(16, 16);
        let mut g = TileGrid::new(16, 16, &mut d);
        g.line((0, 0), (15, 7), 1);
        // Every column gets at least one square, so a fast drag leaves no gap.
        for x in 0..16 {
            assert!((0..16).any(|y| g.get(x, y) == Some(1)), "column {x} has no square");
        }
        // A one-square line is one square.
        let mut d2 = grid(4, 4);
        let mut g2 = TileGrid::new(4, 4, &mut d2);
        g2.line((2, 2), (2, 2), 9);
        assert_eq!(g2.data.iter().filter(|&&p| p == 9).count(), 1);
    }

    /// The one thing a bucket fill must never do: leak through a diagonal.
    #[test]
    fn a_flood_fill_does_not_leak_through_a_diagonal_wall() {
        // A wall of cell 1 running diagonally; fill the top-right region.
        let (cols, rows) = (5u32, 5u32);
        let mut d = grid(cols, rows);
        let mut g = TileGrid::new(cols, rows, &mut d);
        for i in 0..5 {
            g.set(i, i, 1);
        }
        g.flood_fill(4, 0, 2);
        // Above the diagonal is filled…
        assert_eq!(g.get(4, 0), Some(2));
        assert_eq!(g.get(2, 1), Some(2));
        // …and below it is untouched.
        assert_eq!(g.get(0, 4), Some(EMPTY_TILE), "the fill leaked past the diagonal");
        assert_eq!(g.get(1, 4), Some(EMPTY_TILE));
    }

    #[test]
    fn a_flood_fill_of_what_is_already_there_does_nothing() {
        let mut d = vec![3u32; 16];
        let mut g = TileGrid::new(4, 4, &mut d);
        assert!(!g.flood_fill(0, 0, 3), "filling 3 with 3 changes nothing");
        // …including when only the ORIENTATION differs, because a fill matches on
        // the cell. Otherwise it would loop forever re-orienting the same region.
        let turned = tile_pack(3, TileXform::new(1, false));
        assert!(!g.flood_fill(0, 0, turned));
    }

    #[test]
    fn a_flood_fill_treats_one_tile_at_several_orientations_as_one_region() {
        let mut d = grid(3, 1);
        let mut g = TileGrid::new(3, 1, &mut d);
        g.set(0, 0, 5);
        g.set(1, 0, tile_pack(5, TileXform::new(2, false)));
        g.set(2, 0, tile_pack(5, TileXform::new(0, true)));
        assert!(g.flood_fill(0, 0, 8));
        assert!(g.data.iter().all(|&p| p == 8), "all three were the same region");
    }

    #[test]
    fn a_flood_fill_outside_the_grid_is_a_no_op() {
        let mut d = grid(2, 2);
        let mut g = TileGrid::new(2, 2, &mut d);
        assert!(!g.flood_fill(-1, 0, 1));
        assert!(g.data.iter().all(|&p| p == EMPTY_TILE));
    }

    #[test]
    fn replace_all_reaches_disconnected_regions() {
        let mut d = grid(5, 1);
        let mut g = TileGrid::new(5, 1, &mut d);
        g.set(0, 0, 1);
        g.set(4, 0, 1);
        assert!(g.replace_all(0, 0, 2));
        assert_eq!(g.get(0, 0), Some(2));
        assert_eq!(g.get(4, 0), Some(2), "a replace is not connected-only");
    }

    #[test]
    fn a_copied_rectangle_stamps_back_identically() {
        let mut d = grid(6, 6);
        let mut g = TileGrid::new(6, 6, &mut d);
        g.set(1, 1, 3);
        g.set(2, 1, tile_pack(4, TileXform::new(1, true)));
        g.set(2, 2, 5);
        let s = g.copy_rect((1, 1), (2, 2));
        assert_eq!((s.cols, s.rows), (2, 2));
        g.stamp(4, 4, &s, false);
        assert_eq!(g.get(4, 4), Some(3));
        assert_eq!(g.get(5, 4), g.get(2, 1), "the orientation came with it");
        assert_eq!(g.get(5, 5), Some(5));
        assert_eq!(g.get(4, 5), Some(EMPTY_TILE), "…and so did the hole");
    }

    #[test]
    fn a_stamp_can_paint_through_its_own_holes_or_carry_them() {
        let mut d = vec![9u32; 4];
        let mut g = TileGrid::new(2, 2, &mut d);
        let s = Stamp { cols: 2, rows: 2, data: vec![1, EMPTY_TILE, EMPTY_TILE, 1] };
        g.stamp(0, 0, &s, true);
        assert_eq!(g.get(1, 0), Some(9), "skip_empty leaves the ground showing");
        g.stamp(0, 0, &s, false);
        assert_eq!(g.get(1, 0), Some(EMPTY_TILE), "…and without it, the hole is placed");
    }

    /// A stamp turned a quarter-turn must turn its LAYOUT and each square's own
    /// orientation. Only the first is the bug that makes rotated pipe corners
    /// point the wrong way.
    #[test]
    fn rotating_a_stamp_turns_the_layout_and_every_tile_in_it() {
        // A horizontal 3x1 run.
        let s = Stamp { cols: 3, rows: 1, data: vec![1, 2, 3] };
        let r = s.rotated_cw();
        assert_eq!((r.cols, r.rows), (1, 3), "the layout turned");
        // Clockwise: the leftmost square ends up at the TOP.
        assert_eq!(r.get(0, 0).map(tile_index), Some(1));
        assert_eq!(r.get(0, 2).map(tile_index), Some(3));
        // …and every square carries a quarter-turn of its own.
        for y in 0..3 {
            assert_eq!(
                tile_xform(r.get(0, y).unwrap()),
                TileXform::new(1, false),
                "square {y} kept its old orientation"
            );
        }
        // Four turns is where it began.
        assert_eq!(s.rotated_cw().rotated_cw().rotated_cw().rotated_cw(), s);
    }

    #[test]
    fn mirroring_a_stamp_mirrors_the_layout_and_every_tile() {
        let s = Stamp { cols: 3, rows: 2, data: vec![1, 2, 3, 4, 5, 6] };
        let f = s.flipped_x();
        assert_eq!(f.get(0, 0).map(tile_index), Some(3), "the row reversed");
        assert_eq!(tile_xform(f.get(0, 0).unwrap()), TileXform::new(0, true));
        assert_eq!(s.flipped_x().flipped_x(), s);

        let f = s.flipped_y();
        assert_eq!(f.get(0, 0).map(tile_index), Some(4), "the columns reversed");
        assert_eq!(s.flipped_y().flipped_y(), s);
    }

    #[test]
    fn a_stamp_never_turns_a_hole_into_a_tile() {
        let s = Stamp { cols: 2, rows: 2, data: vec![1, EMPTY_TILE, EMPTY_TILE, 2] };
        for turned in [s.rotated_cw(), s.flipped_x(), s.flipped_y()] {
            assert_eq!(
                turned.data.iter().filter(|&&p| p == EMPTY_TILE).count(),
                2,
                "a rotated hole must stay a hole"
            );
        }
    }

    #[test]
    fn a_palette_rectangle_reads_the_sheet_row_major() {
        // A 2x2 block starting at cell (1, 1) of an 8-wide sheet.
        let s = Stamp::from_sheet(8, 1, 1, 2, 2);
        assert_eq!(s.data, vec![9, 10, 17, 18]);
        assert_eq!(Stamp::from_sheet(4, 0, 0, 1, 1).data, vec![0]);
        // A zero-size selection is one tile, not an empty stamp nobody can paint with.
        assert_eq!(Stamp::from_sheet(4, 2, 0, 0, 0).data, vec![2]);
    }

    /// Moving a region onto ground it overlaps must not smear it.
    #[test]
    fn moving_a_region_over_itself_does_not_smear() {
        let mut d = grid(8, 1);
        let mut g = TileGrid::new(8, 1, &mut d);
        for x in 0..4 {
            g.set(x, 0, (x + 1) as u32);
        }
        g.move_rect((0, 0), (3, 0), 1, 0);
        assert_eq!(g.get(0, 0), Some(EMPTY_TILE), "the source square vacated");
        let moved: Vec<Option<u32>> = (1..5).map(|x| g.get(x, 0)).collect();
        assert_eq!(moved, vec![Some(1), Some(2), Some(3), Some(4)], "the run moved intact");
    }

    #[test]
    fn a_zero_move_is_a_no_op() {
        let mut d = vec![1u32, 2, 3, 4];
        let before = d.clone();
        let mut g = TileGrid::new(4, 1, &mut d);
        assert!(!g.move_rect((0, 0), (3, 0), 0, 0));
        assert_eq!(d, before);
    }

    #[test]
    fn reorienting_a_rectangle_keeps_the_cells() {
        let mut d = vec![1u32, 2, 3, 4];
        let mut g = TileGrid::new(4, 1, &mut d);
        g.reorient_rect((0, 0), (3, 0), TileXform::new(1, true));
        for (i, &p) in g.data.iter().enumerate() {
            assert_eq!(tile_index(p), i as u32 + 1, "cell {i} changed");
            assert_eq!(tile_xform(p), TileXform::new(1, true));
        }
    }

    #[test]
    fn resizing_keeps_the_overlap_and_can_grow_in_any_direction() {
        let mut d = vec![1u32, 2, 3, 4];
        let g = TileGrid::new(2, 2, &mut d);
        // Grow right/down: the old content stays at the top-left.
        let (c, r, out) = g.resized(3, 3, 0, 0);
        assert_eq!((c, r), (3, 3));
        assert_eq!(out[0], 1);
        assert_eq!(out[4], 4, "old (1,1) is new (1,1)");
        assert_eq!(out[8], EMPTY_TILE);
        // Grow up/left: the old content slides to (1, 1).
        let (_, _, out) = g.resized(3, 3, 1, 1);
        assert_eq!(out[4], 1, "old (0,0) is new (1,1)");
        assert_eq!(out[0], EMPTY_TILE);
        // Shrinking drops what falls outside rather than refusing.
        let (_, _, out) = g.resized(1, 1, 0, 0);
        assert_eq!(out, vec![1]);
    }

    #[test]
    fn used_bounds_is_the_tightest_box_or_nothing() {
        let mut d = grid(6, 6);
        let mut g = TileGrid::new(6, 6, &mut d);
        assert_eq!(g.used_bounds(), None, "an empty map has no content");
        g.set(2, 1, 1);
        g.set(4, 3, 1);
        assert_eq!(g.used_bounds(), Some((2, 1, 4, 3)));
    }

    // ---- autotiling over a real grid ---------------------------------------

    fn edge4_set() -> TileSet {
        let mut set = TileSet { sheet_cols: 8, sheet_rows: 4, ..Default::default() };
        set.groups.push(AutotileGroup {
            name: "path".into(),
            kind: AutotileKind::Edge4,
            joins: vec![],
        });
        for (cell, mask) in assign_preset(AutotileKind::Edge4, &(0..16).collect::<Vec<_>>()) {
            let info = set.info_mut(cell);
            info.group = Some(0);
            info.mask = mask;
        }
        set
    }

    /// Off the map counts as filled, so a shape painted to the edge does not grow
    /// a coastline against the border.
    #[test]
    fn the_border_of_the_map_is_not_a_hole() {
        let set = edge4_set();
        let mut d = vec![0u32; 9];
        let g = TileGrid::new(3, 3, &mut d);
        // The top-left corner has two neighbours in the grid (E, S) and two off
        // the map (N, W) — all four read as filled.
        assert_eq!(g.neighbour_mask(0, 0, 0, &set) & EDGES, EDGES);
        // A hole IS a hole, though.
        let mut d = vec![0u32; 9];
        let mut g = TileGrid::new(3, 3, &mut d);
        g.set(1, 0, EMPTY_TILE); // north of centre
        assert_eq!(g.neighbour_mask(1, 1, 0, &set) & N, 0, "the hole above is not filled");
        assert_eq!(g.neighbour_mask(1, 1, 0, &set) & EDGES, E | S | W);
    }

    #[test]
    fn painting_a_blob_retiles_its_edges() {
        let set = edge4_set();
        let at = Autotiler::build(&set);
        // A 3x3 filled block in a 5x5 map: the centre sees all four, the edges see
        // three, the corners two.
        let mut d = grid(5, 5);
        let mut g = TileGrid::new(5, 5, &mut d);
        g.fill_rect((1, 1), (3, 3), 0);
        g.retile((1, 1), (3, 3), &set, &at);

        let mask_of = |cell: u32| set.info(cell).unwrap().mask;
        assert_eq!(mask_of(tile_index(g.get(2, 2).unwrap())), EDGES, "the centre is surrounded");
        assert_eq!(mask_of(tile_index(g.get(2, 1).unwrap())), E | S | W, "the top edge has no north");
        assert_eq!(mask_of(tile_index(g.get(1, 1).unwrap())), E | S, "the top-left corner");
        assert_eq!(mask_of(tile_index(g.get(3, 3).unwrap())), N | W, "the bottom-right corner");
    }

    /// The one-ring: a retile of the squares you painted must also fix the ones
    /// around them, or every stroke leaves a seam of stale edges.
    #[test]
    fn a_retile_fixes_the_ring_around_what_was_painted() {
        let set = edge4_set();
        let at = Autotiler::build(&set);
        let mut d = grid(5, 5);
        let mut g = TileGrid::new(5, 5, &mut d);
        // A single square, retiled: it is alone, so its mask is 0 except for the
        // map border.
        g.set(2, 2, 0);
        g.retile((2, 2), (2, 2), &set, &at);
        let lone = tile_index(g.get(2, 2).unwrap());
        assert_eq!(set.info(lone).unwrap().mask, 0, "a lone square has no neighbours");

        // Now paint the square to its east and retile ONLY that square. The
        // one-ring must update the first square to see a neighbour to the east.
        g.set(3, 2, 0);
        g.retile((3, 2), (3, 2), &set, &at);
        let west = tile_index(g.get(2, 2).unwrap());
        assert_eq!(set.info(west).unwrap().mask, E, "the neighbour was not retiled");
    }

    #[test]
    fn a_retile_is_independent_of_scan_order() {
        let set = edge4_set();
        let at = Autotiler::build(&set);
        // The same shape, painted in two different orders, must retile the same.
        let build = |order: &[(i32, i32)]| {
            let mut d = grid(6, 6);
            let mut g = TileGrid::new(6, 6, &mut d);
            for &(x, y) in order {
                g.set(x, y, 0);
            }
            g.retile((0, 0), (5, 5), &set, &at);
            d
        };
        let cells = [(1, 1), (2, 1), (3, 1), (2, 2), (2, 3)];
        let mut reversed: Vec<(i32, i32)> = cells.to_vec();
        reversed.reverse();
        assert_eq!(build(&cells), build(&reversed));
    }

    /// A half-drawn group leaves what it cannot answer alone. It must never erase.
    #[test]
    fn a_retile_never_erases_what_it_cannot_answer() {
        let mut set = edge4_set();
        // Un-assign the tile that answers "surrounded on all four sides".
        let all = set
            .tiles
            .iter()
            .find(|(_, t)| t.mask == EDGES)
            .map(|(c, _)| *c)
            .expect("the preset has an all-four tile");
        set.info_mut(all).group = None;
        let at = Autotiler::build(&set);

        let mut d = grid(5, 5);
        let mut g = TileGrid::new(5, 5, &mut d);
        g.fill_rect((1, 1), (3, 3), 0);
        g.retile((1, 1), (3, 3), &set, &at);
        // The centre square wanted the missing tile; it kept whatever it had.
        assert_ne!(g.get(2, 2), Some(EMPTY_TILE), "the centre was erased");
    }

    /// A hand-turned autotile square keeps its orientation through a retile.
    #[test]
    fn a_retile_keeps_an_orientation_somebody_set_by_hand() {
        let set = edge4_set();
        let at = Autotiler::build(&set);
        let mut d = grid(4, 4);
        let mut g = TileGrid::new(4, 4, &mut d);
        g.fill_rect((1, 1), (2, 2), 0);
        g.set(1, 1, tile_pack(0, TileXform::new(2, true)));
        g.retile((1, 1), (2, 2), &set, &at);
        assert_eq!(
            tile_xform(g.get(1, 1).unwrap()),
            TileXform::new(2, true),
            "the retile overwrote a hand-set orientation"
        );
    }

    #[test]
    fn tile_mask_reports_the_canonical_mask() {
        let mut set = TileSet { sheet_cols: 8, sheet_rows: 8, ..Default::default() };
        set.groups.push(AutotileGroup { name: "g".into(), kind: AutotileKind::Edge4, joins: vec![] });
        let info = set.info_mut(3);
        info.group = Some(0);
        info.mask = N | crate::autotile::NE; // a corner bit Edge4 ignores
        assert_eq!(tile_mask(&set, 3), Some((0, N)), "Edge4 drops the corner");
        assert_eq!(tile_mask(&set, 4), None, "an unassigned tile has no mask");
    }
}
