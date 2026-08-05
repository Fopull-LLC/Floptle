//! What one tilemap square *is*: a cell index plus an orientation, packed into
//! the `u32` a [`crate::Matter::Tilemap`]'s `data` has always held.
//!
//! ## Why the orientation is packed rather than stored beside the index
//!
//! A tile sheet is drawn once and used in four directions. Every 2D tool solves
//! this the same way — Tiled, LDtk and Godot all pack orientation flags into the
//! high bits of the tile id — and the reason is not thrift. A parallel array of
//! orientations is a second thing to keep the same length as `data`, and the
//! moment one of them is resized, or loaded from an older scene, or written by a
//! script that only knew about the first, the two disagree and the map draws
//! garbage that looks like an art bug.
//!
//! So the square is ONE number, and every path that already carried a cell
//! index carries the orientation for free — including a `.ron` scene written
//! before this existed, because an unrotated tile's flag bits are zero.
//!
//! ## The encoding
//!
//! ```text
//!  bit 31    30 29     28 .......... 0
//!  ┌──────┐ ┌───────┐ ┌────────────────┐
//!  │flipX │ │  rot  │ │   cell index   │
//!  └──────┘ └───────┘ └────────────────┘
//! ```
//!
//! `rot` is quarter-turns clockwise (0–3). 536,870,911 cell indices remain,
//! which is more sheet than any GPU will sample.
//!
//! [`crate::EMPTY_TILE`] is `u32::MAX`, so it reads back as an orientation too —
//! that is harmless and deliberate: emptiness is checked before orientation
//! everywhere, and reserving a *separate* sentinel per orientation would have
//! made four ways to say "nothing here".
//!
//! ## The orientation is the dihedral group of the square, and the API says so
//!
//! There are exactly eight ways to place a square tile: four rotations, each
//! optionally mirrored. Which means three independent booleans (`flipX`,
//! `flipY`, `rotate`) *cannot* be the representation — `flipY` is not
//! independent, it is `flipX` composed with a half-turn. Storing all three
//! invites the bug where a game sets `flipY`, reads it back, and gets `false`
//! because something normalised it on the way through.
//!
//! So [`TileXform`] is `(rot, flip_x)` — the eight states, named once — and a
//! vertical flip is a *composition* you ask for ([`TileXform::flipped_y`]),
//! never a field. Reads are canonical, and the editor's ⇔ / ⇕ / ↻ buttons
//! compose through the same functions a script does.

/// The bits of a packed square that hold the cell index.
pub const TILE_CELL_MASK: u32 = 0x1FFF_FFFF;

/// The bits that hold the orientation.
pub const TILE_XFORM_MASK: u32 = !TILE_CELL_MASK;

/// Mirror about the vertical axis — left and right swap. The *first* of the
/// eight states after the four rotations, and the only mirror that needs a bit:
/// every other reflection is this one composed with a rotation.
pub const TILE_FLIP_X: u32 = 1 << 31;

/// Quarter-turns clockwise live here (two bits, 0–3).
pub const TILE_ROT_SHIFT: u32 = 29;

/// One square's orientation: `rot` quarter-turns clockwise, then mirrored about
/// the vertical axis if `flip_x`.
///
/// Rotate-then-mirror, in that order — stated because the other order is a
/// different eight-element labelling of the same eight states, and a script that
/// assumed the other one would place half its tiles wrong in a way that looks
/// like an art mistake.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TileXform {
    /// Quarter-turns clockwise, 0–3. Values outside that range are folded.
    pub rot: u8,
    /// Mirrored left-to-right, applied after the rotation.
    pub flip_x: bool,
}

impl TileXform {
    /// The identity: as the tile is drawn in the sheet.
    pub const NONE: TileXform = TileXform { rot: 0, flip_x: false };

    pub fn new(rot: u8, flip_x: bool) -> Self {
        Self { rot: rot & 3, flip_x }
    }

    /// This orientation with one more quarter-turn clockwise.
    ///
    /// Turning a *mirrored* tile turns what you see, not what the sheet holds —
    /// so the rotation goes the other way under the mirror. Without that, the ↻
    /// button appears to turn anticlockwise as soon as you have flipped the
    /// stamp, which reads as a broken button.
    pub fn rotated_cw(self) -> Self {
        let step = if self.flip_x { 3 } else { 1 };
        Self { rot: (self.rot + step) & 3, flip_x: self.flip_x }
    }

    /// This orientation mirrored left-to-right (about the vertical axis).
    ///
    /// The stored form is *rotate, then mirror*, so mirroring what is already on
    /// screen is exactly toggling the mirror bit — the rotation is untouched.
    pub fn flipped_x(self) -> Self {
        Self { rot: self.rot, flip_x: !self.flip_x }
    }

    /// This orientation mirrored top-to-bottom.
    ///
    /// A half-turn away from [`flipped_x`](Self::flipped_x) — there is no
    /// separate bit for it, because there is no separate *state* for it.
    pub fn flipped_y(self) -> Self {
        Self { rot: (self.rot + 2) & 3, flip_x: !self.flip_x }
    }

    /// True when this orientation mirrors the tile (an odd number of
    /// reflections) — the thing that flips a triangle's handedness, which is why
    /// the collision shapes mirror with it.
    pub fn mirrored(self) -> bool {
        self.flip_x
    }

    pub fn bits(self) -> u32 {
        ((self.rot as u32 & 3) << TILE_ROT_SHIFT) | if self.flip_x { TILE_FLIP_X } else { 0 }
    }

    pub fn from_bits(v: u32) -> Self {
        Self { rot: ((v >> TILE_ROT_SHIFT) & 3) as u8, flip_x: v & TILE_FLIP_X != 0 }
    }

    /// A short label for a UI: `"↻90 ⇔"`, or `"—"` for the identity.
    pub fn label(self) -> String {
        match (self.rot, self.flip_x) {
            (0, false) => "—".into(),
            (r, false) => format!("↻{}", r as u32 * 90),
            (0, true) => "⇔".into(),
            (r, true) => format!("↻{} ⇔", r as u32 * 90),
        }
    }

    /// Every one of the eight states, rotations first.
    pub const ALL: [TileXform; 8] = [
        TileXform { rot: 0, flip_x: false },
        TileXform { rot: 1, flip_x: false },
        TileXform { rot: 2, flip_x: false },
        TileXform { rot: 3, flip_x: false },
        TileXform { rot: 0, flip_x: true },
        TileXform { rot: 1, flip_x: true },
        TileXform { rot: 2, flip_x: true },
        TileXform { rot: 3, flip_x: true },
    ];
}

/// The cell index a packed square draws, orientation stripped.
///
/// Note this does NOT tell you whether the square is empty — an index past the
/// end of the sheet is empty too, which is how [`crate::EMPTY_TILE`] works
/// without a special case. Use [`tile_is_empty`].
pub fn tile_index(packed: u32) -> u32 {
    packed & TILE_CELL_MASK
}

/// The orientation a packed square draws with.
pub fn tile_xform(packed: u32) -> TileXform {
    TileXform::from_bits(packed)
}

/// Pack a cell index and an orientation into one square.
///
/// An index too large for [`TILE_CELL_MASK`] would silently become a *different*
/// tile, so it clamps to empty instead — the same choice the rest of the tile
/// path makes, and the one that shows up as a hole rather than as the wrong art.
pub fn tile_pack(index: u32, xf: TileXform) -> u32 {
    if index > TILE_CELL_MASK {
        return crate::EMPTY_TILE;
    }
    index | xf.bits()
}

/// The same square with a different orientation, keeping its cell.
///
/// An empty square stays empty: re-orienting nothing is nothing, and returning a
/// rotated `EMPTY_TILE` would quietly turn a hole into cell 536,870,911.
pub fn tile_reoriented(packed: u32, xf: TileXform) -> u32 {
    if packed == crate::EMPTY_TILE {
        return crate::EMPTY_TILE;
    }
    tile_pack(tile_index(packed), xf)
}

/// Whether a packed square draws nothing, given how many cells the sheet has.
///
/// `cells` is the sheet's `cols * rows`. Two things are empty: the explicit
/// [`crate::EMPTY_TILE`], and any index past the end of the sheet — which is
/// what makes a map survive its artist cropping the spritesheet.
pub fn tile_is_empty(packed: u32, cells: u32) -> bool {
    packed == crate::EMPTY_TILE || tile_index(packed) >= cells
}

// --- pages: more than one image behind one grid (`floptle/0092`) ------------

/// How many low bits of the cell index address a cell WITHIN one sheet. The
/// bits above it are the sheet — the "page" — the cell lives on.
///
/// ## Why a fixed stride rather than packing the pages end to end
///
/// The alternative is `base(p) = sum of the cells of pages 0..p`, which wastes
/// nothing and renumbers everything: add art to page 0 and every cell on every
/// later page means a different tile, so a level saved yesterday draws garbage.
/// A level is the expensive artifact here and the index space is free —
/// 536 million cells is more sheet than any project — so the stride is fixed and
/// a page's numbering is nailed down the moment it exists.
///
/// 65,536 cells is a 256x256 sheet, which is past what a GPU will hold as one
/// texture; 8,192 pages is past what anybody will draw.
pub const TILE_PAGE_BITS: u32 = 16;

/// Cells addressable on one page.
pub const TILE_PAGE_STRIDE: u32 = 1 << TILE_PAGE_BITS;

/// How many pages the cell index has room for.
pub const TILE_MAX_PAGES: u32 = (TILE_CELL_MASK + 1) >> TILE_PAGE_BITS;

/// Which sheet a cell index lives on. Page 0 is the first sheet, and every
/// index written before pages existed is on it — which is what makes this
/// change invisible to a scene saved earlier.
pub fn tile_page(cell: u32) -> u32 {
    (cell & TILE_CELL_MASK) >> TILE_PAGE_BITS
}

/// Where a cell sits within its own page, row-major from the top-left, exactly
/// as a single-sheet cell index always has.
pub fn tile_in_page(cell: u32) -> u32 {
    cell & (TILE_PAGE_STRIDE - 1)
}

/// The cell index of the `index`-th cell of `page`.
///
/// Out of range in either argument gives [`crate::EMPTY_TILE`] rather than
/// wrapping into a different page — a wrong tile drawn confidently is worse
/// than a hole.
pub fn tile_cell_of(page: u32, index: u32) -> u32 {
    if page >= TILE_MAX_PAGES || index >= TILE_PAGE_STRIDE {
        return crate::EMPTY_TILE;
    }
    page * TILE_PAGE_STRIDE + index
}

/// Which corner of the tile's ART a given corner of the drawn quad samples,
/// under `xf`.
///
/// Both coordinates are `0` or `1`; `s` runs left → right and `t` runs bottom →
/// top, in both spaces. This is the single place the orientation is *applied*:
/// the mesh builder calls it per corner, and the editor's palette preview calls
/// it to draw the same tile the same way, so a stamp cannot preview one
/// orientation and place another.
pub fn tile_corner(s: u8, t: u8, xf: TileXform) -> (u8, u8) {
    // The stored orientation is "rotate the art, then mirror it", so walking
    // back from screen space to art space undoes the mirror first.
    let mut s = if xf.flip_x { 1 - s } else { s };
    let mut t = t;
    // Then un-rotate, a quarter-turn at a time. Drawing the art a quarter-turn
    // CLOCKWISE sends art (a, b) to screen (b, 1 - a); finding which art corner
    // landed here therefore runs the other way: (s, t) <- (1 - t, s).
    for _ in 0..(xf.rot & 3) {
        let (ns, nt) = (1 - t, s);
        s = ns;
        t = nt;
    }
    (s, t)
}

/// Where the art's corner `(a, b)` is DRAWN under `xf` — the forward direction
/// of [`tile_corner`], which is the one a diagram or a collision shape needs.
pub fn tile_corner_drawn(a: u8, b: u8, xf: TileXform) -> (u8, u8) {
    let (s, t) = tile_point_drawn(a as f32, b as f32, xf);
    (s as u8, t as u8)
}

/// Where a point inside the tile is DRAWN under `xf`, in the unit square.
///
/// The continuous version of [`tile_corner_drawn`], and the reason it exists is
/// collision: a tile whose collider is the bottom half must collide across the
/// *left* half once it is turned a quarter-turn, and a half-height box is not
/// something a corner permutation can express. Because the eight orientations
/// are symmetries of the square, an axis-aligned rect maps to an axis-aligned
/// rect exactly — no bounds slop, no rounding.
pub fn tile_point_drawn(a: f32, b: f32, xf: TileXform) -> (f32, f32) {
    let mut a = a;
    let mut b = b;
    for _ in 0..(xf.rot & 3) {
        let (na, nb) = (b, 1.0 - a);
        a = na;
        b = nb;
    }
    (if xf.flip_x { 1.0 - a } else { a }, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EMPTY_TILE;

    #[test]
    fn a_plain_cell_packs_to_itself() {
        // The compatibility guarantee: every tilemap written before orientations
        // existed is a list of bare indices, and must keep drawing identically.
        for cell in [0u32, 1, 7, 63, 4095] {
            assert_eq!(tile_pack(cell, TileXform::NONE), cell);
            assert_eq!(tile_index(cell), cell);
            assert_eq!(tile_xform(cell), TileXform::NONE);
        }
    }

    #[test]
    fn empty_stays_empty_through_every_orientation() {
        for xf in TileXform::ALL {
            assert_eq!(
                tile_reoriented(EMPTY_TILE, xf),
                EMPTY_TILE,
                "rotating a hole must not turn it into a tile"
            );
        }
        // …and the sheet-size test agrees, whatever bits are set.
        assert!(tile_is_empty(EMPTY_TILE, 64));
        assert!(tile_is_empty(tile_pack(99, TileXform::new(1, true)), 64), "past the sheet");
        assert!(!tile_is_empty(tile_pack(63, TileXform::new(1, true)), 64), "last real cell");
    }

    #[test]
    fn index_and_orientation_survive_a_round_trip() {
        for cell in [0u32, 5, 4095, TILE_CELL_MASK] {
            for xf in TileXform::ALL {
                let p = tile_pack(cell, xf);
                assert_eq!(tile_index(p), cell, "cell {cell} under {xf:?}");
                assert_eq!(tile_xform(p), xf, "orientation of cell {cell}");
            }
        }
    }

    #[test]
    fn an_index_too_big_to_pack_becomes_a_hole_not_another_tile() {
        assert_eq!(tile_pack(TILE_CELL_MASK + 1, TileXform::NONE), EMPTY_TILE);
        assert_eq!(tile_pack(u32::MAX, TileXform::NONE), EMPTY_TILE);
    }

    /// The eight states are a group: four turns is identity, two mirrors is
    /// identity, and every composition lands back inside the eight.
    #[test]
    fn the_orientations_compose_as_the_squares_symmetries() {
        for start in TileXform::ALL {
            let mut x = start;
            for _ in 0..4 {
                x = x.rotated_cw();
            }
            assert_eq!(x, start, "four quarter-turns from {start:?} is where it began");

            assert_eq!(start.flipped_x().flipped_x(), start, "mirroring twice from {start:?}");
            assert_eq!(start.flipped_y().flipped_y(), start, "flipping twice from {start:?}");
            // The identity a vertical flip actually is.
            assert_eq!(
                start.flipped_y(),
                start.flipped_x().rotated_cw().rotated_cw(),
                "⇕ is ⇔ plus a half-turn, from {start:?}"
            );
        }
    }

    /// `↻` must turn what is ON SCREEN clockwise, whether or not the tile is
    /// mirrored. This is the bug where flipping the stamp silently reverses the
    /// rotate button — the two states where `rot` has to count *down* to keep
    /// the picture turning the same way.
    #[test]
    fn rotate_always_turns_the_picture_clockwise() {
        // A quarter-turn clockwise on the drawn square: (s, t) -> (t, 1 - s).
        let cw = |(s, t): (u8, u8)| (t, 1 - s);
        for xf in TileXform::ALL {
            let turned = xf.rotated_cw();
            for a in 0..2 {
                for b in 0..2 {
                    assert_eq!(
                        tile_corner_drawn(a, b, turned),
                        cw(tile_corner_drawn(a, b, xf)),
                        "art corner ({a}, {b}) of {xf:?} must move clockwise on ↻"
                    );
                }
            }
        }
    }

    /// A vertical flip is a vertical flip in *every* starting orientation — the
    /// ⇕ button cannot become a horizontal flip once the tile is turned.
    #[test]
    fn flip_buttons_mirror_the_picture_on_the_expected_axis() {
        let mirror_s = |(s, t): (u8, u8)| (1 - s, t);
        let mirror_t = |(s, t): (u8, u8)| (s, 1 - t);
        for xf in TileXform::ALL {
            for a in 0..2 {
                for b in 0..2 {
                    let at = tile_corner_drawn(a, b, xf);
                    assert_eq!(
                        tile_corner_drawn(a, b, xf.flipped_x()),
                        mirror_s(at),
                        "⇔ on {xf:?} must mirror left-to-right"
                    );
                    assert_eq!(
                        tile_corner_drawn(a, b, xf.flipped_y()),
                        mirror_t(at),
                        "⇕ on {xf:?} must mirror top-to-bottom"
                    );
                }
            }
        }
    }

    /// `tile_corner` and `tile_corner_drawn` are each other's inverse. They are
    /// written separately because the mesh needs one direction and a collision
    /// shape the other, and two hand-written inverses that drift is exactly how
    /// a rotated tile ends up drawing one way and colliding another.
    #[test]
    fn the_two_corner_directions_are_inverses() {
        for xf in TileXform::ALL {
            for a in 0..2 {
                for b in 0..2 {
                    let (s, t) = tile_corner_drawn(a, b, xf);
                    assert_eq!(tile_corner(s, t, xf), (a, b), "{xf:?} at art ({a}, {b})");
                }
            }
        }
    }

    #[test]
    fn the_identity_samples_each_corner_from_itself() {
        for s in 0..2 {
            for t in 0..2 {
                assert_eq!(tile_corner(s, t, TileXform::NONE), (s, t));
            }
        }
    }

    /// Every orientation is a bijection on the four corners — a permutation, not
    /// a collapse. A bug that mapped two drawn corners to one art corner would
    /// draw a tile folded in half, and it would only show on rotated tiles.
    #[test]
    fn every_orientation_permutes_the_four_corners() {
        for xf in TileXform::ALL {
            let mut seen = std::collections::HashSet::new();
            for s in 0..2 {
                for t in 0..2 {
                    assert!(seen.insert(tile_corner(s, t, xf)), "{xf:?} folds a corner onto another");
                }
            }
            assert_eq!(seen.len(), 4);
        }
    }

    #[test]
    fn a_mirror_is_a_mirror_and_a_turn_is_not() {
        assert!(!TileXform::NONE.mirrored());
        assert!(!TileXform::NONE.rotated_cw().mirrored());
        assert!(TileXform::NONE.flipped_x().mirrored());
        assert!(TileXform::NONE.flipped_y().mirrored());
        assert!(!TileXform::NONE.flipped_x().flipped_y().mirrored(), "two mirrors is a turn");
    }
}
