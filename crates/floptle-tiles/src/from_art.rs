//! A tile's collision, read off its pixels.
//!
//! Drawing a collider for every tile of a sheet is the tedious half of
//! setting up a level, and for most tiles the art already says the answer:
//! a solid block of ground is solid, a tuft of grass is not, a ramp is the
//! shape of its top edge. This makes that first guess, so a freshly dropped
//! spritesheet collides like it looks, and an author only fixes the tiles it
//! got wrong.
//!
//! The rules, in order:
//!
//! 1. **Almost nothing there** (under 4% of the tile's pixels): walk through.
//! 2. **Almost all of it** (90% or more, touching every edge): the whole
//!    square. These merge into long boxes, so they are worth recognising.
//! 3. **Ground shape**: every column that has art runs unbroken from its top
//!    pixel down to the tile's bottom edge, and the tops differ by more than a
//!    quarter tile. That is a slope or a bumpy floor, and it becomes an
//!    outline that follows the top edge.
//! 4. **Anything else**: the tightest box around the art.
//!
//! "Art" is a pixel at least half opaque.

use crate::tileset::TileCollision;

/// The guessed collision for one cell of an RGBA image cut `cols` × `rows`.
/// `None` when the cell is outside the image.
pub fn collision_from_art(px: &[u8], w: u32, h: u32, cols: u32, rows: u32, cell: u32) -> Option<TileCollision> {
    let (cols, rows) = (cols.max(1), rows.max(1));
    if cell >= cols * rows || w < cols || h < rows || px.len() < (w * h * 4) as usize {
        return None;
    }
    let (cw, ch) = (w / cols, h / rows);
    let (ox, oy) = ((cell % cols) * cw, (cell / cols) * ch);
    // Opacity, with y counted UP from the tile's bottom so it matches the
    // collision frame.
    let solid = |x: u32, y_up: u32| -> bool {
        let (gx, gy) = (ox + x, oy + (ch - 1 - y_up));
        px[((gy * w + gx) * 4 + 3) as usize] >= 128
    };
    let mut count = 0u32;
    let (mut x0, mut y0, mut x1, mut y1) = (cw, ch, 0u32, 0u32);
    for y in 0..ch {
        for x in 0..cw {
            if solid(x, y) {
                count += 1;
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x + 1);
                y1 = y1.max(y + 1);
            }
        }
    }
    let area = cw * ch;
    if count * 25 < area {
        return Some(TileCollision::None);
    }
    if count * 10 >= area * 9 && x0 == 0 && y0 == 0 && x1 == cw && y1 == ch {
        return Some(TileCollision::Full);
    }
    let (fw, fh) = (cw as f32, ch as f32);
    // Ground shape: each column with art is solid from its top to the bottom.
    let mut tops: Vec<Option<u32>> = Vec::with_capacity(cw as usize);
    let mut ground = y0 == 0;
    for x in 0..cw {
        let top = (0..ch).rev().find(|&y| solid(x, y));
        if let Some(t) = top
            && (0..=t).any(|y| !solid(x, y))
        {
            ground = false;
            break;
        }
        tops.push(top.map(|t| t + 1));
    }
    if ground {
        let heights: Vec<u32> = tops.iter().filter_map(|t| *t).collect();
        let (lo, hi) = (heights.iter().min().copied().unwrap_or(0), heights.iter().max().copied().unwrap_or(0));
        if (hi - lo) * 4 > ch {
            // The outline: along the bottom, then back along the top edge from
            // right to left, simplified so a straight ramp is three points and
            // not one per pixel column.
            let first = tops.iter().position(|t| t.is_some()).unwrap_or(0) as u32;
            let last = tops.iter().rposition(|t| t.is_some()).unwrap_or(0) as u32 + 1;
            let mut top_edge: Vec<[f32; 2]> = Vec::new();
            for x in (first..last).rev() {
                let t = tops[x as usize].unwrap_or(0) as f32 / fh;
                top_edge.push([(x + 1) as f32 / fw, t]);
                top_edge.push([x as f32 / fw, t]);
            }
            let top_edge = simplify(&top_edge, 0.75 / fw.max(fh));
            let mut pts = vec![[first as f32 / fw, 0.0], [last as f32 / fw, 0.0]];
            pts.extend(top_edge);
            pts.dedup();
            if pts.len() >= 3 {
                return Some(TileCollision::Poly(pts));
            }
        }
    }
    Some(TileCollision::Custom {
        x: x0 as f32 / fw,
        y: y0 as f32 / fh,
        w: (x1 - x0) as f32 / fw,
        h: (y1 - y0) as f32 / fh,
    })
}

/// Ramer–Douglas–Peucker: drop every point within `eps` of the line its
/// neighbours make.
fn simplify(pts: &[[f32; 2]], eps: f32) -> Vec<[f32; 2]> {
    if pts.len() < 3 {
        return pts.to_vec();
    }
    let (a, b) = (pts[0], pts[pts.len() - 1]);
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len = (dx * dx + dy * dy).sqrt().max(1e-9);
    let (mut worst, mut at) = (0.0f32, 0);
    for (i, p) in pts.iter().enumerate().take(pts.len() - 1).skip(1) {
        let d = ((p[0] - a[0]) * dy - (p[1] - a[1]) * dx).abs() / len;
        if d > worst {
            worst = d;
            at = i;
        }
    }
    if worst <= eps {
        return vec![a, b];
    }
    let mut left = simplify(&pts[..=at], eps);
    let right = simplify(&pts[at..], eps);
    left.pop();
    left.extend(right);
    left
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tileset::TileShape;

    /// A 4×1 sheet of 16-px tiles, each drawn by `f(cell, x, y_up)`.
    fn sheet(f: impl Fn(u32, u32, u32) -> bool) -> Vec<u8> {
        let (w, h) = (64u32, 16u32);
        let mut px = vec![0u8; (w * h * 4) as usize];
        for gy in 0..h {
            for gx in 0..w {
                if f(gx / 16, gx % 16, 15 - gy) {
                    px[((gy * w + gx) * 4 + 3) as usize] = 255;
                }
            }
        }
        px
    }

    fn guess(px: &[u8], cell: u32) -> TileCollision {
        collision_from_art(px, 64, 16, 4, 1, cell).unwrap()
    }

    #[test]
    fn a_tile_collides_like_it_looks() {
        let px = sheet(|cell, x, y| match cell {
            0 => true,                 // a block of ground
            1 => x == 7 && y < 2,      // a two-pixel tuft
            2 => y <= x,               // a ramp up to the right
            _ => (6..10).contains(&y), // a floating ledge
        });
        assert_eq!(guess(&px, 0), TileCollision::Full);
        assert_eq!(guess(&px, 1), TileCollision::None);
        match guess(&px, 2) {
            TileCollision::Poly(pts) => {
                assert!(pts.len() <= 5, "a straight ramp simplifies to a few points: {pts:?}");
                let s = TileCollision::Poly(pts.clone());
                assert!(matches!(s.shape(), TileShape::Poly(_)));
                // Low on the left, high on the right.
                let top_left = pts.iter().filter(|p| p[0] < 0.2).map(|p| p[1]).fold(0.0f32, f32::max);
                let top_right = pts.iter().filter(|p| p[0] > 0.8).map(|p| p[1]).fold(0.0f32, f32::max);
                assert!(top_right > 0.9 && top_left < 0.2, "{pts:?}");
            }
            other => panic!("a ramp must be an outline, got {other:?}"),
        }
        assert_eq!(
            guess(&px, 3),
            TileCollision::Custom { x: 0.0, y: 6.0 / 16.0, w: 1.0, h: 4.0 / 16.0 },
            "a ledge off the ground is the box around it"
        );
    }

    #[test]
    fn a_cell_outside_the_image_has_no_answer() {
        let px = sheet(|_, _, _| true);
        assert_eq!(collision_from_art(&px, 64, 16, 4, 1, 4), None);
    }
}
