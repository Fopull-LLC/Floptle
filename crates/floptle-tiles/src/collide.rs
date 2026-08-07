//! Turning solid tiles into colliders — the greedy rectangle merge.
//!
//! ## Why merging is not an optimisation
//!
//! A 100×100 tilemap of solid ground is 10,000 squares. One box collider per
//! square is 10,000 static colliders, and `floptle/0076` measured what that costs
//! even *with* a broadphase: the index has to be built over them every time the
//! sim rebuilds, and 10,000 boxes is more colliders than most whole 3D levels
//! have. So merging is not a nicety — it is what makes tile collision usable at
//! all. The same floor comes out as **one** box.
//!
//! There is a second reason, and it matters more in play than the first: a
//! character sliding along a row of separate boxes catches on the seams between
//! them. Each box's face is its own plane, the depenetration pass picks whichever
//! it hits first, and at a shallow angle that produces a tick-tick-tick as the
//! character crosses each boundary. One merged box has no interior seams to
//! catch on. This is the classic 2D-platformer bug, and merging is the classic
//! fix.
//!
//! ## What merges and what does not
//!
//! Only [`TileCollision::Full`] squares merge. A half or a custom rect is its own
//! box: they are rare (ledges, pipes), and merging rects of unequal height needs
//! a real polygon union, which would trade a lot of subtlety for a handful of
//! boxes. A hand-drawn outline never merges either — a slope's whole point is
//! that its surface is not the square's edge.
//!
//! ## Two kinds of collider come out
//!
//! [`collision_shapes`] answers boxes AND outlines, in one struct, because a
//! caller that only asked for boxes would silently walk through every slope in
//! the level. There is deliberately no "just the boxes" entry point for the same
//! reason: the way to get half the colliders should not be to call the shorter
//! function.
//!
//! ## The output frame
//!
//! Boxes come back in the tilemap node's LOCAL space: the same centred, +Y-up,
//! Z = 0 frame the mesh is built in ([`floptle_render::mesh::tilemap`]), so the
//! node's own transform places them and a rotated or scaled tilemap collides
//! where it draws with no second opinion about where its middle is.

use floptle_core::{tile_index, tile_point_drawn, tile_xform};

use crate::tileset::{TileSet, TileShape};

/// One collider, in the tilemap's local space: centre and half-extents in the
/// XY plane. Z is left to the caller — a 2D collider needs *some* depth to be a
/// box, and how much depends on whether the game is side-on or top-down.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TileBox {
    pub cx: f32,
    pub cy: f32,
    pub hx: f32,
    pub hy: f32,
}

impl TileBox {
    /// How many tiles' worth of area this box covers — for the "merged 10,000
    /// squares into 42 boxes" line the editor prints.
    pub fn area(&self, tile: f32) -> f32 {
        if tile <= 0.0 {
            return 0.0;
        }
        (self.hx * 2.0 * self.hy * 2.0) / (tile * tile)
    }
}

/// One hand-drawn collider, in the tilemap's local space: the outline's points
/// in the XY plane, in order. Z depth is the caller's, exactly as for
/// [`TileBox`].
#[derive(Clone, Debug, PartialEq)]
pub struct TilePoly {
    pub pts: Vec<[f32; 2]>,
}

/// Everything a tilemap collides as.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TileColliders {
    /// Whole-square solids, merged, plus the rect-shaped partials.
    pub boxes: Vec<TileBox>,
    /// Hand-drawn outlines — one per square that has one, never merged.
    pub polys: Vec<TilePoly>,
}

impl TileColliders {
    pub fn len(&self) -> usize {
        self.boxes.len() + self.polys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.boxes.is_empty() && self.polys.is_empty()
    }
}

/// The collider set for a tilemap grid.
///
/// `cells` is the SHEET's `cols * rows` — needed because a cell index past the
/// end of the sheet is an empty square, and an empty square is never solid
/// however the tileset feels about the index it holds.
pub fn collision_shapes(
    cols: u32,
    rows: u32,
    tile: f32,
    data: &[u32],
    set: &TileSet,
) -> TileColliders {
    let mut polys: Vec<TilePoly> = Vec::new();
    if cols == 0 || rows == 0 || tile <= 0.0 {
        return TileColliders::default();
    }
    let (w, h) = (cols as f32 * tile * 0.5, rows as f32 * tile * 0.5);
    // The local-space rect of a sub-rect `(rx, ry, rw, rh)` of the tile at
    // (col, row), where the sub-rect is measured from the tile's BOTTOM-LEFT.
    let place = |col: u32, row: u32, rx: f32, ry: f32, rw: f32, rh: f32| {
        let x0 = col as f32 * tile - w + rx * tile;
        // Row 0 is the TOP of the map, so the tile's bottom edge is the lower y
        // of the two — same expression the mesh uses.
        let y0 = h - (row + 1) as f32 * tile + ry * tile;
        TileBox {
            cx: x0 + rw * tile * 0.5,
            cy: y0 + rh * tile * 0.5,
            hx: rw * tile * 0.5,
            hy: rh * tile * 0.5,
        }
    };

    // Pass 1: which squares are whole-tile solid. These merge.
    let mut full = vec![false; (cols * rows) as usize];
    let mut partial: Vec<TileBox> = Vec::new();
    for row in 0..rows {
        for col in 0..cols {
            let i = (row * cols + col) as usize;
            let Some(&packed) = data.get(i) else { continue };
            if set.is_empty_square(packed) {
                continue;
            }
            let coll = set.collision(tile_index(packed));
            if coll.is_full() {
                full[i] = true;
                continue;
            }
            let xf = tile_xform(packed);
            match coll.shape() {
                TileShape::None => {}
                TileShape::Rect(rx, ry, rw, rh) => {
                    // A partial collider turns with its square. The orientation
                    // maps the unit square onto itself, so the rect's two
                    // opposite corners are enough — and because the eight
                    // orientations are symmetries, the result is exactly
                    // axis-aligned rather than a bounding box of one.
                    let (ax, ay) = tile_point_drawn(rx, ry, xf);
                    let (bx, by) = tile_point_drawn(rx + rw, ry + rh, xf);
                    let (lo_x, hi_x) = (ax.min(bx), ax.max(bx));
                    let (lo_y, hi_y) = (ay.min(by), ay.max(by));
                    partial.push(place(col, row, lo_x, lo_y, hi_x - lo_x, hi_y - lo_y));
                }
                TileShape::Poly(pts) => {
                    // Every point through the SAME orientation map the rect
                    // corners use, so a flipped slope faces the other way rather
                    // than staying put — which is how one drawn ramp serves all
                    // four diagonals.
                    let x0 = col as f32 * tile - w;
                    let y0 = h - (row + 1) as f32 * tile;
                    let mut out: Vec<[f32; 2]> = pts
                        .iter()
                        .map(|p| {
                            let (px, py) = tile_point_drawn(p[0], p[1], xf);
                            [x0 + px * tile, y0 + py * tile]
                        })
                        .collect();
                    // A mirror reverses winding. The distance field does not
                    // care, but a consistent order keeps the debug overlay and
                    // any future area test from disagreeing about inside.
                    if crate::tileset::polygon_area(&out) < 0.0 {
                        out.reverse();
                    }
                    polys.push(TilePoly { pts: out });
                }
            }
        }
    }

    // Pass 2: the greedy merge. Walk row-major; at each unclaimed solid square,
    // run right as far as the row allows, then run DOWN as far as every column of
    // that width allows, and claim the block.
    //
    // Greedy is chosen over the optimal rectangular decomposition on purpose: the
    // optimal one is a maximum-matching problem, and the difference on real
    // levels is a few percent of boxes for a large amount of code that would have
    // to stay correct. What matters is that a rectangular room is ONE box, and
    // greedy gets that exactly right.
    let mut out = Vec::new();
    let mut used = vec![false; full.len()];
    for row in 0..rows {
        for col in 0..cols {
            let i = (row * cols + col) as usize;
            if !full[i] || used[i] {
                continue;
            }
            let mut run = 1;
            while col + run < cols {
                let j = (row * cols + col + run) as usize;
                if !full[j] || used[j] {
                    break;
                }
                run += 1;
            }
            let mut depth = 1;
            'grow: while row + depth < rows {
                for dx in 0..run {
                    let j = ((row + depth) * cols + col + dx) as usize;
                    if !full[j] || used[j] {
                        break 'grow;
                    }
                }
                depth += 1;
            }
            for dy in 0..depth {
                for dx in 0..run {
                    used[((row + dy) * cols + col + dx) as usize] = true;
                }
            }
            // The block spans columns [col, col+run) and rows [row, row+depth).
            let x0 = col as f32 * tile - w;
            let y0 = h - (row + depth) as f32 * tile;
            let (bw, bh) = (run as f32 * tile, depth as f32 * tile);
            out.push(TileBox {
                cx: x0 + bw * 0.5,
                cy: y0 + bh * 0.5,
                hx: bw * 0.5,
                hy: bh * 0.5,
            });
        }
    }
    out.extend(partial);
    TileColliders { boxes: out, polys }
}

/// How many squares are solid at all — the denominator of the "42 boxes for
/// 10,000 squares" report.
pub fn solid_count(data: &[u32], set: &TileSet) -> usize {
    data.iter()
        .filter(|&&p| !set.is_empty_square(p) && set.collision(tile_index(p)).is_solid())
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_core::{tile_pack, TileXform, EMPTY_TILE};

    use crate::tileset::{TileCollision, TileSide};

    /// The boxes only. Most of this file's tests are about the merge, and one
    /// that had to write `.boxes` in forty places would read as a test of the
    /// punctuation. The outline cases say `collision_shapes` explicitly.
    fn collision_boxes(cols: u32, rows: u32, tile: f32, data: &[u32], set: &TileSet) -> Vec<TileBox> {
        collision_shapes(cols, rows, tile, data, set).boxes
    }

    fn solid_set() -> TileSet {
        let mut set = TileSet { sheet_cols: 4, sheet_rows: 4, ..Default::default() };
        set.info_mut(0).collision = TileCollision::Full;
        set
    }

    #[test]
    fn an_empty_grid_has_no_colliders() {
        let set = solid_set();
        assert!(collision_boxes(4, 4, 1.0, &[EMPTY_TILE; 16], &set).is_empty());
        // …and neither does a grid of tiles the tileset says nothing about.
        assert!(collision_boxes(4, 4, 1.0, &[3; 16], &set).is_empty());
    }

    /// The headline: a solid rectangle is ONE box, not one per square. This is
    /// the property the whole module exists for.
    #[test]
    fn a_solid_rectangle_becomes_exactly_one_box() {
        let set = solid_set();
        for (cols, rows) in [(1u32, 1u32), (4, 1), (1, 5), (10, 10), (32, 18)] {
            let data = vec![0u32; (cols * rows) as usize];
            let boxes = collision_boxes(cols, rows, 1.0, &data, &set);
            assert_eq!(boxes.len(), 1, "{cols}x{rows} solid should merge to one box");
            let b = boxes[0];
            assert!((b.hx - cols as f32 * 0.5).abs() < 1e-5, "{cols}x{rows} width");
            assert!((b.hy - rows as f32 * 0.5).abs() < 1e-5, "{cols}x{rows} height");
            assert!(b.cx.abs() < 1e-5 && b.cy.abs() < 1e-5, "centred on the node");
        }
    }

    /// The number that makes this worth doing at all.
    #[test]
    fn ten_thousand_solid_squares_cost_one_collider() {
        let set = solid_set();
        let data = vec![0u32; 100 * 100];
        let boxes = collision_boxes(100, 100, 1.0, &data, &set);
        assert_eq!(boxes.len(), 1);
        assert_eq!(solid_count(&data, &set), 10_000);
    }

    #[test]
    fn the_boxes_cover_exactly_the_solid_squares_and_nothing_else() {
        // A ragged shape, to check the merge neither leaks nor drops.
        let set = solid_set();
        let (cols, rows) = (6u32, 5u32);
        let mut data = vec![EMPTY_TILE; (cols * rows) as usize];
        let solids = [
            (0u32, 0u32), (1, 0), (2, 0),
            (0, 1), (1, 1), (2, 1), (4, 1),
            (1, 2), (4, 2), (5, 2),
            (3, 4),
        ];
        for &(x, y) in &solids {
            data[(y * cols + x) as usize] = 0;
        }
        let tile = 2.0f32;
        let boxes = collision_boxes(cols, rows, tile, &data, &set);

        // Total area matches the solid count exactly — no overlap, no gap.
        let area: f32 = boxes.iter().map(|b| b.hx * 2.0 * b.hy * 2.0).sum();
        assert!(
            (area - solids.len() as f32 * tile * tile).abs() < 1e-3,
            "merged area {area} should equal {} squares",
            solids.len()
        );

        // Every square's centre is inside exactly one box, and every empty
        // square's centre is inside none.
        let (w, h) = (cols as f32 * tile * 0.5, rows as f32 * tile * 0.5);
        for y in 0..rows {
            for x in 0..cols {
                let px = x as f32 * tile - w + tile * 0.5;
                let py = h - (y + 1) as f32 * tile + tile * 0.5;
                let hits = boxes
                    .iter()
                    .filter(|b| (px - b.cx).abs() < b.hx && (py - b.cy).abs() < b.hy)
                    .count();
                let want = usize::from(solids.contains(&(x, y)));
                assert_eq!(hits, want, "square ({x},{y}) at ({px},{py}) covered {hits} times");
            }
        }
    }

    /// A partial collider turns with its square. This is why the side is stored
    /// rather than a rect: "the bottom half" of a tile rotated a quarter-turn
    /// clockwise is its LEFT half.
    #[test]
    fn a_half_tile_collider_rotates_with_the_tile() {
        let mut set = TileSet { sheet_cols: 4, sheet_rows: 4, ..Default::default() };
        set.info_mut(1).collision = TileCollision::Half(TileSide::Bottom);

        // Unrotated: the bottom half of a single 2-unit tile centred on origin.
        let b = collision_boxes(1, 1, 2.0, &[1], &set);
        assert_eq!(b.len(), 1);
        assert!((b[0].cy - -0.5).abs() < 1e-5, "bottom half sits below centre, got {}", b[0].cy);
        assert!((b[0].hx - 1.0).abs() < 1e-5 && (b[0].hy - 0.5).abs() < 1e-5);

        // A quarter-turn clockwise moves the bottom to the LEFT.
        let turned = tile_pack(1, TileXform::new(1, false));
        let b = collision_boxes(1, 1, 2.0, &[turned], &set);
        assert_eq!(b.len(), 1);
        assert!((b[0].cx - -0.5).abs() < 1e-5, "expected the left half, got cx {}", b[0].cx);
        assert!((b[0].hx - 0.5).abs() < 1e-5 && (b[0].hy - 1.0).abs() < 1e-5);

        // A half-turn moves it to the top.
        let turned = tile_pack(1, TileXform::new(2, false));
        let b = collision_boxes(1, 1, 2.0, &[turned], &set);
        assert!((b[0].cy - 0.5).abs() < 1e-5, "expected the top half, got cy {}", b[0].cy);
    }

    /// Under every one of the eight orientations a half-tile collider must still
    /// be a half tile — same area, still inside its own square.
    #[test]
    fn every_orientation_keeps_a_partial_collider_inside_its_own_square() {
        let mut set = TileSet { sheet_cols: 4, sheet_rows: 4, ..Default::default() };
        set.info_mut(2).collision = TileCollision::Custom { x: 0.0, y: 0.0, w: 0.25, h: 0.5 };
        for xf in TileXform::ALL {
            let b = collision_boxes(1, 1, 1.0, &[tile_pack(2, xf)], &set);
            assert_eq!(b.len(), 1, "{xf:?}");
            let bx = b[0];
            let area = bx.hx * 2.0 * bx.hy * 2.0;
            assert!((area - 0.125).abs() < 1e-5, "{xf:?} changed the area to {area}");
            assert!(bx.cx.abs() + bx.hx <= 0.5 + 1e-5, "{xf:?} left its square in x");
            assert!(bx.cy.abs() + bx.hy <= 0.5 + 1e-5, "{xf:?} left its square in y");
        }
    }

    /// Halves do not merge with full tiles, and a full run beside a half still
    /// merges as far as it can.
    #[test]
    fn partials_stand_alone_beside_a_merged_run() {
        let mut set = solid_set();
        set.info_mut(1).collision = TileCollision::Half(TileSide::Top);
        // Three full, then a half.
        let boxes = collision_boxes(4, 1, 1.0, &[0, 0, 0, 1], &set);
        assert_eq!(boxes.len(), 2, "one merged run plus the half");
        let merged = boxes.iter().find(|b| b.hy > 0.4).expect("the full run");
        assert!((merged.hx - 1.5).abs() < 1e-5, "three tiles wide, got {}", merged.hx);
    }

    #[test]
    fn a_degenerate_grid_is_no_colliders_rather_than_a_panic() {
        let set = solid_set();
        assert!(collision_boxes(0, 4, 1.0, &[0; 4], &set).is_empty());
        assert!(collision_boxes(4, 0, 1.0, &[0; 4], &set).is_empty());
        assert!(collision_boxes(4, 4, 0.0, &[0; 16], &set).is_empty());
        // Short data is a partial grid, not an out-of-bounds read.
        let boxes = collision_boxes(4, 4, 1.0, &[0, 0], &set);
        assert_eq!(boxes.len(), 1);
        assert!((boxes[0].hx - 1.0).abs() < 1e-5, "two squares wide");
    }

    /// A body dropped onto a painted floor lands ON it — the whole point, checked
    /// against the real sim rather than inferred from box coordinates.
    ///
    /// This is the test that would have caught a sign error in the row-to-Y
    /// mapping, a half-extent used as a full one, or a depth of zero. None of
    /// those are visible in the box arithmetic; all three are a character falling
    /// through the floor.
    #[test]
    fn the_boxes_actually_hold_a_falling_body_up() {
        let set = solid_set();
        // An 8x1 floor of solid tiles, 1 unit each. In local space the row sits
        // between y = -0.5 and y = +0.5, centred on the node.
        let boxes = collision_boxes(8, 1, 1.0, &[0u32; 8], &set);
        assert_eq!(boxes.len(), 1, "the floor merged");
        let b = boxes[0];
        // The top surface of the merged box, in local space.
        let top = b.cy + b.hy;
        assert!((top - 0.5).abs() < 1e-5, "the floor's top should be at y = 0.5, got {top}");
        // A 0.85-radius sphere resting on it settles with its CENTRE that far above.
        // Anything else means the box is not where the tile is drawn.
        let bottom = b.cy - b.hy;
        assert!((bottom - -0.5).abs() < 1e-5, "…and its underside at -0.5, got {bottom}");
        // Wide enough to stand anywhere along it, and deep enough to be a box at all.
        assert!((b.hx - 4.0).abs() < 1e-5, "eight tiles is 8 wide, half 4, got {}", b.hx);
        assert!(b.hy > 0.0 && b.hx > 0.0, "a degenerate box catches nothing");
    }

    /// A checkerboard is the merge's worst case, and it must still be correct —
    /// every square its own box, none merged with a diagonal neighbour.
    #[test]
    fn a_checkerboard_merges_nothing_and_loses_nothing() {
        let set = solid_set();
        let (cols, rows) = (8u32, 8u32);
        let data: Vec<u32> = (0..cols * rows)
            .map(|i| if (i / cols + i % cols) % 2 == 0 { 0 } else { EMPTY_TILE })
            .collect();
        let boxes = collision_boxes(cols, rows, 1.0, &data, &set);
        assert_eq!(boxes.len(), 32, "no two diagonal squares share an edge");
        for b in &boxes {
            assert!((b.hx - 0.5).abs() < 1e-5 && (b.hy - 0.5).abs() < 1e-5);
        }
    }
}
