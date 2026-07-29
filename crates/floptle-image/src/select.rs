//! Selections and layer masks — one 8-bit coverage buffer, used for both.
//!
//! A selection clips **every** subsequent operation (brush, fill, filter,
//! adjustment, transform, delete), which is the difference between a toy and a
//! real editor. It's 8-bit rather than 1-bit so feathering, gradients-as-masks
//! and anti-aliased lassos are the same object as a hard marquee.

use crate::tiles::TileGrid;
use crate::Rect;

/// A full-canvas 8-bit coverage buffer. 255 = fully selected.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Mask {
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}

/// How a new marquee combines with the live selection.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SelectOp {
    #[default]
    Replace,
    Add,
    Subtract,
    Intersect,
}

impl Mask {
    pub fn new(w: u32, h: u32, value: u8) -> Self {
        Mask { w: w.max(1), h: h.max(1), data: vec![value; (w.max(1) * h.max(1)) as usize] }
    }

    #[inline]
    pub fn get(&self, x: i32, y: i32) -> u8 {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return 0;
        }
        self.data[y as usize * self.w as usize + x as usize]
    }

    /// Coverage as 0..1 — what every operation multiplies its strength by.
    #[inline]
    pub fn at(&self, x: i32, y: i32) -> f32 {
        self.get(x, y) as f32 / 255.0
    }

    #[inline]
    pub fn set(&mut self, x: i32, y: i32, v: u8) {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return;
        }
        let w = self.w as usize;
        self.data[y as usize * w + x as usize] = v;
    }

    pub fn bounds(&self) -> Rect {
        Rect::size(self.w, self.h)
    }

    /// True when nothing is selected at all (so callers can treat it as "no selection").
    pub fn is_empty(&self) -> bool {
        self.data.iter().all(|&v| v == 0)
    }

    pub fn is_full(&self) -> bool {
        self.data.iter().all(|&v| v == 255)
    }

    /// Bounding box of the selected region (`EMPTY` when nothing is selected) — the
    /// rect every clipped operation can restrict itself to.
    pub fn selected_bounds(&self) -> Rect {
        let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for y in 0..self.h as i32 {
            for x in 0..self.w as i32 {
                if self.get(x, y) > 0 {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
        }
        if x1 < x0 {
            Rect::EMPTY
        } else {
            Rect::from_points(x0, y0, x1, y1)
        }
    }

    /// Combine `other` into `self` under `op`.
    pub fn combine(&mut self, other: &Mask, op: SelectOp) {
        for i in 0..self.data.len().min(other.data.len()) {
            let (a, b) = (self.data[i] as u16, other.data[i] as u16);
            self.data[i] = match op {
                SelectOp::Replace => b as u8,
                SelectOp::Add => a.max(b) as u8,
                SelectOp::Subtract => a.saturating_sub(b) as u8,
                SelectOp::Intersect => a.min(b) as u8,
            };
        }
    }

    pub fn invert(&mut self) {
        for v in &mut self.data {
            *v = 255 - *v;
        }
    }

    /// Box-blur the coverage `radius` pixels — feather.
    pub fn feather(&mut self, radius: u32) {
        if radius == 0 {
            return;
        }
        let r = radius as i32;
        let (w, h) = (self.w as i32, self.h as i32);
        // Separable box blur, twice, for a smoother falloff than one pass.
        for _ in 0..2 {
            let mut tmp = vec![0u8; self.data.len()];
            for y in 0..h {
                let mut sum: u32 = 0;
                for x in -r..=r {
                    sum += self.get(x, y) as u32;
                }
                for x in 0..w {
                    tmp[(y * w + x) as usize] = (sum / (2 * r as u32 + 1)) as u8;
                    sum = sum + self.get(x + r + 1, y) as u32 - self.get(x - r, y) as u32;
                }
            }
            self.data = tmp;
            let mut tmp = vec![0u8; self.data.len()];
            for x in 0..w {
                let mut sum: u32 = 0;
                for y in -r..=r {
                    sum += self.get(x, y) as u32;
                }
                for y in 0..h {
                    tmp[(y * w + x) as usize] = (sum / (2 * r as u32 + 1)) as u8;
                    sum = sum + self.get(x, y + r + 1) as u32 - self.get(x, y - r) as u32;
                }
            }
            self.data = tmp;
        }
    }

    /// Grow (positive) or shrink (negative) the selection by `n` pixels, hard-edged.
    pub fn expand(&mut self, n: i32) {
        if n == 0 {
            return;
        }
        let (w, h) = (self.w as i32, self.h as i32);
        let grow = n > 0;
        let r = n.abs();
        let mut out = vec![0u8; self.data.len()];
        for y in 0..h {
            for x in 0..w {
                let mut v: u8 = if grow { 0 } else { 255 };
                'k: for dy in -r..=r {
                    for dx in -r..=r {
                        if dx * dx + dy * dy > r * r {
                            continue;
                        }
                        let s = self.get(x + dx, y + dy);
                        if grow {
                            v = v.max(s);
                            if v == 255 {
                                break 'k;
                            }
                        } else {
                            v = v.min(s);
                            if v == 0 {
                                break 'k;
                            }
                        }
                    }
                }
                out[(y * w + x) as usize] = v;
            }
        }
        self.data = out;
    }

    /// Resize the mask onto a new canvas (nearest, anchored at 0,0).
    /// Mirror the mask in place. A canvas flip that left its masks alone would
    /// move every pixel and leave what hides them behind.
    pub fn flip(&mut self, horizontal: bool) {
        let (w, h) = (self.w as usize, self.h as usize);
        if horizontal {
            for y in 0..h {
                self.data[y * w..(y + 1) * w].reverse();
            }
        } else {
            for y in 0..h / 2 {
                for x in 0..w {
                    self.data.swap(y * w + x, (h - 1 - y) * w + x);
                }
            }
        }
    }

    /// A copy rotated by `turns` quarter-turns clockwise; odd turns swap w/h.
    /// Mirrors `transform::rotate_quarter` exactly, so pixels and their masks
    /// can never come out of a rotation disagreeing.
    pub fn rotated(&self, turns: i32) -> Mask {
        let t = turns.rem_euclid(4);
        if t == 0 {
            return self.clone();
        }
        let (w, h) = (self.w as usize, self.h as usize);
        let (nw, nh) = if t % 2 == 1 { (h, w) } else { (w, h) };
        let mut out = Mask { w: nw as u32, h: nh as u32, data: vec![0; nw * nh] };
        for y in 0..h {
            for x in 0..w {
                let (dx, dy) = match t {
                    1 => (h - 1 - y, x),
                    2 => (w - 1 - x, h - 1 - y),
                    _ => (y, w - 1 - x),
                };
                out.data[dy * nw + dx] = self.data[y * w + x];
            }
        }
        out
    }

    pub fn recanvased(&self, w: u32, h: u32, dx: i32, dy: i32) -> Mask {
        let mut out = Mask::new(w, h, 0);
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                out.set(x, y, self.get(x - dx, y - dy));
            }
        }
        out
    }
}

/// A hard rectangular marquee.
pub fn rect_mask(w: u32, h: u32, r: Rect) -> Mask {
    let mut m = Mask::new(w, h, 0);
    for y in r.y..r.bottom() {
        for x in r.x..r.right() {
            m.set(x, y, 255);
        }
    }
    m
}

/// An anti-aliased elliptical marquee inscribed in `r`.
pub fn ellipse_mask(w: u32, h: u32, r: Rect) -> Mask {
    let mut m = Mask::new(w, h, 0);
    if r.is_empty() {
        return m;
    }
    let cx = r.x as f32 + r.w as f32 / 2.0;
    let cy = r.y as f32 + r.h as f32 / 2.0;
    let rx = (r.w as f32 / 2.0).max(0.5);
    let ry = (r.h as f32 / 2.0).max(0.5);
    for y in r.y - 1..r.bottom() + 1 {
        for x in r.x - 1..r.right() + 1 {
            // 4×4 supersample so the edge isn't a staircase.
            let mut hits = 0;
            for sy in 0..4 {
                for sx in 0..4 {
                    let px = x as f32 + (sx as f32 + 0.5) / 4.0;
                    let py = y as f32 + (sy as f32 + 0.5) / 4.0;
                    let dx = (px - cx) / rx;
                    let dy = (py - cy) / ry;
                    if dx * dx + dy * dy <= 1.0 {
                        hits += 1;
                    }
                }
            }
            if hits > 0 {
                m.set(x, y, (hits * 255 / 16) as u8);
            }
        }
    }
    m
}

/// A closed polygon (the lasso), even-odd filled with 4× vertical supersampling.
pub fn polygon_mask(w: u32, h: u32, pts: &[(f32, f32)]) -> Mask {
    let mut m = Mask::new(w, h, 0);
    if pts.len() < 3 {
        return m;
    }
    let mut acc = vec![0u16; w as usize];
    for y in 0..h as i32 {
        acc.iter_mut().for_each(|v| *v = 0);
        for s in 0..4 {
            let sy = y as f32 + (s as f32 + 0.5) / 4.0;
            let mut xs: Vec<f32> = Vec::new();
            for i in 0..pts.len() {
                let (x0, y0) = pts[i];
                let (x1, y1) = pts[(i + 1) % pts.len()];
                if (y0 <= sy && y1 > sy) || (y1 <= sy && y0 > sy) {
                    let t = (sy - y0) / (y1 - y0);
                    xs.push(x0 + t * (x1 - x0));
                }
            }
            xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            for pair in xs.chunks_exact(2) {
                let (a, b) = (pair[0], pair[1]);
                let x0 = a.floor().max(0.0) as usize;
                let x1 = (b.ceil() as i64).clamp(0, w as i64) as usize;
                for (x, slot) in acc.iter_mut().enumerate().take(x1).skip(x0) {
                    let cov = (b.min(x as f32 + 1.0) - a.max(x as f32)).clamp(0.0, 1.0);
                    // 64 per sub-scanline: four of them reach 256, clamped to a
                    // solid 255 below. (63.75 truncated to 63 and a fully-covered
                    // pixel came out at 252 — visibly not selected.)
                    *slot += (cov * 64.0).round() as u16;
                }
            }
        }
        for x in 0..w as i32 {
            let v = acc[x as usize].min(255) as u8;
            if v > 0 {
                m.set(x, y, v);
            }
        }
    }
    m
}

/// Magic wand: flood (or global) select every pixel within `tolerance` of the
/// colour at (`sx`, `sy`). Tolerance is 0..255 over the max channel difference,
/// alpha included — so it picks "the transparent background" as readily as a
/// flat colour.
pub fn wand_mask(g: &TileGrid, sx: i32, sy: i32, tolerance: u8, contiguous: bool) -> Mask {
    let (w, h) = (g.width(), g.height());
    let mut m = Mask::new(w, h, 0);
    let seed = g.get(sx as i64, sy as i64);
    let close = |px: [u8; 4]| -> bool {
        (0..4).all(|i| (px[i] as i32 - seed[i] as i32).unsigned_abs() as u8 <= tolerance)
            // Two fully-transparent pixels match regardless of their stale RGB.
            || (px[3] == 0 && seed[3] == 0)
    };
    if !contiguous {
        for y in 0..h as i32 {
            for x in 0..w as i32 {
                if close(g.get(x as i64, y as i64)) {
                    m.set(x, y, 255);
                }
            }
        }
        return m;
    }
    if sx < 0 || sy < 0 || sx >= w as i32 || sy >= h as i32 {
        return m;
    }
    let mut stack = vec![(sx, sy)];
    m.set(sx, sy, 255);
    while let Some((x, y)) = stack.pop() {
        for (dx, dy) in [(1, 0), (-1, 0), (0, 1), (0, -1)] {
            let (nx, ny) = (x + dx, y + dy);
            if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                continue;
            }
            if m.get(nx, ny) != 0 {
                continue;
            }
            if close(g.get(nx as i64, ny as i64)) {
                m.set(nx, ny, 255);
                stack.push((nx, ny));
            }
        }
    }
    m
}

/// Select by colour range over the whole canvas, with a soft falloff over
/// `tolerance` — the "select every shade of sky" tool.
pub fn color_range_mask(g: &TileGrid, color: [u8; 4], tolerance: u8) -> Mask {
    let (w, h) = (g.width(), g.height());
    let mut m = Mask::new(w, h, 0);
    let tol = (tolerance as f32).max(1.0);
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let px = g.get(x as i64, y as i64);
            let d = (0..3)
                .map(|i| (px[i] as f32 - color[i] as f32).abs())
                .fold(0.0f32, f32::max);
            let a = (px[3] as f32 - color[3] as f32).abs();
            let d = d.max(a);
            if d <= tol {
                m.set(x, y, crate::u8c((1.0 - d / tol) * 255.0));
            }
        }
    }
    m
}

/// A layer's alpha, as a mask — "select the sprite, not the background".
pub fn alpha_mask(g: &TileGrid) -> Mask {
    let (w, h) = (g.width(), g.height());
    let mut m = Mask::new(w, h, 0);
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            m.set(x, y, g.get(x as i64, y as i64)[3]);
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boolean_ops_behave() {
        let a = rect_mask(10, 10, Rect::new(0, 0, 6, 10));
        let b = rect_mask(10, 10, Rect::new(4, 0, 6, 10));
        let mut u = a.clone();
        u.combine(&b, SelectOp::Add);
        assert!(u.is_full());
        let mut i = a.clone();
        i.combine(&b, SelectOp::Intersect);
        assert_eq!(i.selected_bounds(), Rect::new(4, 0, 2, 10));
        let mut s = a.clone();
        s.combine(&b, SelectOp::Subtract);
        assert_eq!(s.selected_bounds(), Rect::new(0, 0, 4, 10));
        let mut inv = a.clone();
        inv.invert();
        assert_eq!(inv.selected_bounds(), Rect::new(6, 0, 4, 10));
    }

    #[test]
    fn feather_softens_the_edge_without_moving_the_middle() {
        let mut m = rect_mask(40, 40, Rect::new(10, 10, 20, 20));
        m.feather(3);
        assert_eq!(m.get(20, 20), 255, "the middle stays solid");
        assert!(m.get(10, 20) < 255 && m.get(10, 20) > 0, "the edge ramps");
        assert!(m.get(8, 20) > 0, "coverage spreads outward");
    }

    #[test]
    fn grow_and_shrink() {
        let mut m = rect_mask(40, 40, Rect::new(10, 10, 10, 10));
        m.expand(2);
        assert_eq!(m.selected_bounds(), Rect::new(8, 8, 14, 14));
        m.expand(-2);
        assert_eq!(m.selected_bounds(), Rect::new(10, 10, 10, 10));
    }

    #[test]
    fn wand_is_contiguous_when_asked() {
        let mut g = TileGrid::new(20, 10);
        // Two separate red blobs.
        g.edit_rect(Rect::new(0, 0, 5, 10), |_, _, px| *px = [255, 0, 0, 255]);
        g.edit_rect(Rect::new(15, 0, 5, 10), |_, _, px| *px = [255, 0, 0, 255]);
        let m = wand_mask(&g, 2, 2, 0, true);
        assert_eq!(m.selected_bounds(), Rect::new(0, 0, 5, 10));
        let m = wand_mask(&g, 2, 2, 0, false);
        assert_eq!(m.selected_bounds(), Rect::new(0, 0, 20, 10));
    }

    #[test]
    fn polygon_fills_a_triangle() {
        let m = polygon_mask(20, 20, &[(2.0, 2.0), (18.0, 2.0), (2.0, 18.0)]);
        assert_eq!(m.get(4, 4), 255);
        assert_eq!(m.get(17, 17), 0);
    }

    #[test]
    fn ellipse_is_antialiased_and_inside_its_box() {
        let m = ellipse_mask(40, 40, Rect::new(5, 5, 30, 30));
        assert_eq!(m.get(20, 20), 255);
        assert_eq!(m.get(5, 5), 0, "corners of the box are outside the ellipse");
        assert_eq!(m.selected_bounds().intersect(Rect::new(4, 4, 32, 32)), m.selected_bounds());
    }
}
