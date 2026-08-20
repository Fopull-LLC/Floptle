//! Tiled RGBA8 pixel storage with copy-on-write blocks.
//!
//! A layer is a grid of 128² tiles, each `Option<Arc<Vec<u8>>>`. Absent = fully
//! transparent and costs nothing, so a 4096² document with a corner painted uses
//! the memory of that corner. Writes go through `Arc::make_mut`, so **cloning a
//! whole grid is O(tiles), not O(pixels)** — which is what makes "snapshot the
//! document for undo on every stroke" affordable (§11.4 of the proposal).
//!
//! Coordinates are i64 so a brush can be dragged off-canvas without wrapping;
//! out-of-bounds reads are transparent and out-of-bounds writes are dropped.

use std::sync::Arc;

use crate::Rect;

/// Tile edge in pixels.
pub const TILE: usize = 128;
const TILE_BYTES: usize = TILE * TILE * 4;

#[derive(Clone, Debug)]
pub struct TileGrid {
    w: u32,
    h: u32,
    cols: usize,
    rows: usize,
    tiles: Vec<Option<Arc<Vec<u8>>>>,
}

impl TileGrid {
    /// An all-transparent grid. Allocates only the tile index.
    pub fn new(w: u32, h: u32) -> Self {
        let (w, h) = (w.max(1), h.max(1));
        let cols = w.div_ceil(TILE as u32) as usize;
        let rows = h.div_ceil(TILE as u32) as usize;
        TileGrid { w, h, cols, rows, tiles: vec![None; cols * rows] }
    }

    /// A grid filled from a tightly-packed RGBA8 buffer (`w*h*4` bytes).
    pub fn from_rgba(w: u32, h: u32, px: &[u8]) -> Self {
        let mut g = TileGrid::new(w, h);
        if px.len() < (w as usize * h as usize * 4) {
            return g;
        }
        g.write_rgba(Rect::size(w, h), px, w as usize * 4);
        g
    }

    /// A grid filled with one solid colour.
    pub fn filled(w: u32, h: u32, color: [u8; 4]) -> Self {
        let mut g = TileGrid::new(w, h);
        g.fill(color);
        g
    }

    pub fn width(&self) -> u32 {
        self.w
    }
    pub fn height(&self) -> u32 {
        self.h
    }
    pub fn bounds(&self) -> Rect {
        Rect::size(self.w, self.h)
    }

    /// Number of allocated (non-empty) tiles — memory accounting for the UI.
    pub fn resident_tiles(&self) -> usize {
        self.tiles.iter().filter(|t| t.is_some()).count()
    }

    #[inline]
    fn tile_index(&self, tx: usize, ty: usize) -> usize {
        ty * self.cols + tx
    }

    /// The pixel at (x, y); transparent outside the canvas.
    #[inline]
    pub fn get(&self, x: i64, y: i64) -> [u8; 4] {
        if x < 0 || y < 0 || x >= self.w as i64 || y >= self.h as i64 {
            return [0, 0, 0, 0];
        }
        let (tx, ty) = (x as usize / TILE, y as usize / TILE);
        let Some(t) = &self.tiles[self.tile_index(tx, ty)] else { return [0, 0, 0, 0] };
        let o = ((y as usize % TILE) * TILE + (x as usize % TILE)) * 4;
        [t[o], t[o + 1], t[o + 2], t[o + 3]]
    }

    /// Like [`get`](Self::get) but wrapping at the edges — the sampler tiling mode
    /// uses, so a seam-crossing filter reads the same texels the GPU will.
    #[inline]
    pub fn get_wrapped(&self, x: i64, y: i64) -> [u8; 4] {
        let x = x.rem_euclid(self.w as i64);
        let y = y.rem_euclid(self.h as i64);
        self.get(x, y)
    }

    /// Write one pixel (no blending). Ignored outside the canvas.
    #[inline]
    pub fn set(&mut self, x: i64, y: i64, px: [u8; 4]) {
        if x < 0 || y < 0 || x >= self.w as i64 || y >= self.h as i64 {
            return;
        }
        let (tx, ty) = (x as usize / TILE, y as usize / TILE);
        let i = self.tile_index(tx, ty);
        let o = ((y as usize % TILE) * TILE + (x as usize % TILE)) * 4;
        // Skip the copy-on-write entirely when writing transparent to an absent tile.
        if self.tiles[i].is_none() && px[3] == 0 && px[0] == 0 && px[1] == 0 && px[2] == 0 {
            return;
        }
        let t = self.tiles[i].get_or_insert_with(|| Arc::new(vec![0u8; TILE_BYTES]));
        Arc::make_mut(t)[o..o + 4].copy_from_slice(&px);
    }

    /// Run `f(x, y, &mut px)` over every pixel of `rect` ∩ canvas, walking each
    /// affected tile ONCE (one copy-on-write per tile, not per pixel). This is the
    /// hot path every brush dab and filter goes through.
    pub fn edit_rect(&mut self, rect: Rect, mut f: impl FnMut(i32, i32, &mut [u8; 4])) {
        let r = rect.intersect(self.bounds());
        if r.is_empty() {
            return;
        }
        let tx0 = r.x as usize / TILE;
        let ty0 = r.y as usize / TILE;
        let tx1 = (r.right() - 1) as usize / TILE;
        let ty1 = (r.bottom() - 1) as usize / TILE;
        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                let i = self.tile_index(tx, ty);
                let tile = self.tiles[i].get_or_insert_with(|| Arc::new(vec![0u8; TILE_BYTES]));
                let data = Arc::make_mut(tile);
                let ox = (tx * TILE) as i32;
                let oy = (ty * TILE) as i32;
                let x0 = r.x.max(ox);
                let y0 = r.y.max(oy);
                let x1 = r.right().min(ox + TILE as i32);
                let y1 = r.bottom().min(oy + TILE as i32);
                for y in y0..y1 {
                    let row = (y - oy) as usize * TILE;
                    for x in x0..x1 {
                        let o = (row + (x - ox) as usize) * 4;
                        let mut px = [data[o], data[o + 1], data[o + 2], data[o + 3]];
                        f(x, y, &mut px);
                        data[o..o + 4].copy_from_slice(&px);
                    }
                }
            }
        }
    }

    /// Copy `rect` into a tightly-packed buffer (`rect.w*rect.h*4`). Pixels outside
    /// the canvas come back transparent, so callers can read a margin safely.
    pub fn read_rect(&self, rect: Rect) -> Vec<u8> {
        let mut out = vec![0u8; rect.w as usize * rect.h as usize * 4];
        for row in 0..rect.h as i32 {
            for col in 0..rect.w as i32 {
                let px = self.get((rect.x + col) as i64, (rect.y + row) as i64);
                let o = (row as usize * rect.w as usize + col as usize) * 4;
                out[o..o + 4].copy_from_slice(&px);
            }
        }
        out
    }

    /// Overwrite `rect` from a buffer with `stride` bytes per row.
    pub fn write_rgba(&mut self, rect: Rect, src: &[u8], stride: usize) {
        let ox = rect.x;
        let oy = rect.y;
        self.edit_rect(rect, |x, y, px| {
            let o = (y - oy) as usize * stride + (x - ox) as usize * 4;
            if o + 4 <= src.len() {
                px.copy_from_slice(&src[o..o + 4]);
            }
        });
    }

    /// Flatten the whole grid to a tightly-packed RGBA8 buffer.
    pub fn to_rgba(&self) -> Vec<u8> {
        self.read_rect(self.bounds())
    }

    /// Set every pixel to `color` (dropping tiles entirely when it's transparent).
    pub fn fill(&mut self, color: [u8; 4]) {
        if color == [0, 0, 0, 0] {
            self.clear();
            return;
        }
        let bounds = self.bounds();
        self.edit_rect(bounds, |_, _, px| *px = color);
    }

    /// Drop every tile — free, and the cheapest possible "erase all".
    pub fn clear(&mut self) {
        for t in &mut self.tiles {
            *t = None;
        }
    }

    /// Release tiles that turned out to be fully transparent (after a big erase),
    /// so memory tracks what's actually painted.
    pub fn prune(&mut self) {
        for t in &mut self.tiles {
            if let Some(data) = t
                && data.as_chunks::<4>().0.iter().all(|p| p[3] == 0)
            {
                *t = None;
            }
        }
    }

    /// The tight bounding box of non-transparent pixels, or `EMPTY` when blank.
    pub fn opaque_bounds(&self) -> Rect {
        let mut out = Rect::EMPTY;
        for ty in 0..self.rows {
            for tx in 0..self.cols {
                let Some(t) = &self.tiles[self.tile_index(tx, ty)] else { continue };
                for y in 0..TILE {
                    for x in 0..TILE {
                        if t[(y * TILE + x) * 4 + 3] != 0 {
                            let px = (tx * TILE + x) as i32;
                            let py = (ty * TILE + y) as i32;
                            if px < self.w as i32 && py < self.h as i32 {
                                out = out.union(Rect::new(px, py, 1, 1));
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// A copy resized to `w`×`h`, anchored at (`dx`, `dy`) — the canvas-resize /
    /// crop primitive. Pixels that fall outside the new canvas are dropped.
    pub fn recanvased(&self, w: u32, h: u32, dx: i32, dy: i32) -> TileGrid {
        let mut out = TileGrid::new(w, h);
        let r = Rect::new(dx, dy, self.w, self.h).intersect(out.bounds());
        if r.is_empty() {
            return out;
        }
        let (sx, sy) = (r.x - dx, r.y - dy);
        let src = self.read_rect(Rect::new(sx, sy, r.w, r.h));
        out.write_rgba(r, &src, r.w as usize * 4);
        out
    }

    /// True when nothing has ever been written (every tile absent).
    pub fn is_blank(&self) -> bool {
        self.tiles.iter().all(|t| t.is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_grid_reads_transparent_and_allocates_nothing() {
        let g = TileGrid::new(1000, 1000);
        assert_eq!(g.get(500, 500), [0, 0, 0, 0]);
        assert_eq!(g.resident_tiles(), 0);
        assert!(g.is_blank());
    }

    #[test]
    fn writes_allocate_only_the_touched_tile() {
        let mut g = TileGrid::new(1024, 1024); // 8×8 = 64 tiles
        g.set(5, 5, [1, 2, 3, 4]);
        assert_eq!(g.resident_tiles(), 1);
        assert_eq!(g.get(5, 5), [1, 2, 3, 4]);
        g.set(1000, 1000, [9, 9, 9, 9]);
        assert_eq!(g.resident_tiles(), 2);
    }

    #[test]
    fn out_of_bounds_is_transparent_and_ignored() {
        let mut g = TileGrid::new(16, 16);
        g.set(-4, 2, [255, 0, 0, 255]);
        g.set(100, 100, [255, 0, 0, 255]);
        assert_eq!(g.resident_tiles(), 0);
        assert_eq!(g.get(-1, -1), [0, 0, 0, 0]);
        assert_eq!(g.get_wrapped(-1, -1), g.get(15, 15));
    }

    /// The point of the whole module: a clone shares tiles until one is written.
    #[test]
    fn clone_is_copy_on_write() {
        let mut a = TileGrid::new(256, 256);
        a.fill([10, 20, 30, 255]);
        let b = a.clone();
        // Same pixels, and the clone was cheap (Arc bumps only).
        assert_eq!(b.get(200, 200), [10, 20, 30, 255]);
        a.set(200, 200, [99, 0, 0, 255]);
        // The snapshot did NOT follow the edit.
        assert_eq!(b.get(200, 200), [10, 20, 30, 255]);
        assert_eq!(a.get(200, 200), [99, 0, 0, 255]);
    }

    #[test]
    fn edit_rect_covers_exactly_the_rect() {
        let mut g = TileGrid::new(300, 300);
        g.edit_rect(Rect::new(100, 100, 100, 100), |_, _, px| *px = [255, 255, 255, 255]);
        assert_eq!(g.get(99, 150), [0, 0, 0, 0]);
        assert_eq!(g.get(100, 100), [255, 255, 255, 255]);
        assert_eq!(g.get(199, 199), [255, 255, 255, 255]);
        assert_eq!(g.get(200, 200), [0, 0, 0, 0]);
    }

    #[test]
    fn rgba_round_trips() {
        let px: Vec<u8> = (0..(9 * 7 * 4)).map(|i| (i % 251) as u8).collect();
        let g = TileGrid::from_rgba(9, 7, &px);
        assert_eq!(g.to_rgba(), px);
    }

    #[test]
    fn recanvas_crops_and_pads() {
        let mut g = TileGrid::filled(8, 8, [7, 7, 7, 255]);
        g.set(0, 0, [1, 1, 1, 255]);
        let bigger = g.recanvased(16, 16, 4, 4);
        assert_eq!(bigger.get(4, 4), [1, 1, 1, 255]);
        assert_eq!(bigger.get(0, 0), [0, 0, 0, 0]);
        let cropped = g.recanvased(4, 4, -2, -2);
        assert_eq!(cropped.get(0, 0), [7, 7, 7, 255]);
        assert_eq!(cropped.width(), 4);
    }

    #[test]
    fn opaque_bounds_finds_the_painted_box() {
        let mut g = TileGrid::new(300, 300);
        g.set(10, 20, [1, 1, 1, 255]);
        g.set(200, 250, [1, 1, 1, 255]);
        assert_eq!(g.opaque_bounds(), Rect::from_points(10, 20, 200, 250));
        assert!(TileGrid::new(64, 64).opaque_bounds().is_empty());
    }

    #[test]
    fn prune_releases_erased_tiles() {
        let mut g = TileGrid::new(256, 256);
        g.fill([1, 2, 3, 255]);
        assert!(g.resident_tiles() > 0);
        g.edit_rect(g.bounds(), |_, _, px| px[3] = 0);
        g.prune();
        assert_eq!(g.resident_tiles(), 0);
    }
}
