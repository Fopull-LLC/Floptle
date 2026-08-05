//! **Autotiling**: a group of tiles that pick themselves by what is next to
//! them, so you paint a shape and the corners, edges and ends appear.
//!
//! ## The one honest thing about autotile presets
//!
//! Every tool has a different idea of what order a 16- or 47-tile sheet is laid
//! out in, and none of them is more correct than the others. A preset that
//! guesses your artist's order and gets it wrong produces a level of *plausible*
//! wrongness: it tiles, it just tiles with the wrong corners, and it reads as
//! bad art rather than a wrong table.
//!
//! So two things:
//!
//! 1. The presets here are stated exactly ([`preset_masks`]), in ascending mask
//!    order, and documented as such rather than named after a tool.
//! 2. Every tile's mask is drawn in the palette as a little 3×3 diagram of the
//!    neighbourhood it answers. If a preset guessed wrong you can SEE which
//!    tiles disagree, and fixing one is a click. That is the part that makes a
//!    guess safe.
//!
//! ## Masks
//!
//! One bit per neighbour, clockwise from north:
//!
//! ```text
//!   NW  N  NE          128   1   2
//!    W  ·  E     =      64   ·   4
//!   SW  S  SE           32  16   8
//! ```
//!
//! North is up on screen, which for a tilemap means *row - 1* — worth stating,
//! because `data` is row-major from the TOP-LEFT, so the north neighbour is the
//! one at the LOWER row index.
//!
//! ## The corner rule
//!
//! For [`AutotileKind::Blob8`] a corner bit only counts when both of its
//! adjacent edges are also set. Without that rule there are 256 combinations and
//! an artist would have to draw all of them; with it there are 47, and the ones
//! that fall away are exactly the ones that look identical anyway — a diagonal
//! neighbour with no shared edge is not visible from inside the tile.

use crate::tileset::{AutotileKind, TileSet};

pub const N: u8 = 1;
pub const NE: u8 = 2;
pub const E: u8 = 4;
pub const SE: u8 = 8;
pub const S: u8 = 16;
pub const SW: u8 = 32;
pub const W: u8 = 64;
pub const NW: u8 = 128;

/// The four edges.
pub const EDGES: u8 = N | E | S | W;

/// The eight neighbours as `(dx, dy, bit)`, where `dy` is in ROW space (so `-1`
/// is north / up the screen).
pub const OFFSETS: [(i32, i32, u8); 8] = [
    (0, -1, N),
    (1, -1, NE),
    (1, 0, E),
    (1, 1, SE),
    (0, 1, S),
    (-1, 1, SW),
    (-1, 0, W),
    (-1, -1, NW),
];

/// Reduce a raw 8-neighbour mask to the canonical form its kind distinguishes.
///
/// This is where the corner rule lives, and it is applied on BOTH sides — when a
/// preset assigns masks and when a paint looks one up — so the two cannot
/// disagree about what a mask means.
pub fn canonical(kind: AutotileKind, mask: u8) -> u8 {
    match kind {
        AutotileKind::Edge4 => mask & EDGES,
        AutotileKind::Blob8 => {
            let mut out = mask & EDGES;
            for (corner, a, b) in [(NE, N, E), (SE, S, E), (SW, S, W), (NW, N, W)] {
                if mask & corner != 0 && mask & a != 0 && mask & b != 0 {
                    out |= corner;
                }
            }
            out
        }
    }
}

/// Every mask a kind can produce, ascending. 16 for [`AutotileKind::Edge4`], 47
/// for [`AutotileKind::Blob8`].
///
/// This IS the preset: hand a group its `n`th tile and it answers the `n`th mask
/// in this list. Ascending numeric order is chosen because it is the one order
/// that can be *derived* rather than remembered — anybody can regenerate this
/// list, and the palette prints each tile's mask beside it so a mismatch with
/// somebody's sheet is visible rather than mysterious.
pub fn preset_masks(kind: AutotileKind) -> Vec<u8> {
    let mut out: Vec<u8> = (0u16..=255)
        .map(|m| canonical(kind, m as u8))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// How many tiles a kind's preset needs.
pub fn preset_len(kind: AutotileKind) -> usize {
    match kind {
        AutotileKind::Edge4 => 16,
        AutotileKind::Blob8 => 47,
    }
}

/// A resolver built once from a tileset, so painting a stroke does not re-scan
/// the tile table per square.
///
/// Holds, per group, a 256-entry table from raw mask to cell. Raw rather than
/// canonical so a lookup is one index with no branching, and so a group with an
/// incomplete sheet still answers *something* for every neighbourhood (see
/// [`Autotiler::resolve`]).
pub struct Autotiler {
    /// `tables[group][mask]` = the cell to draw, or `u32::MAX` for "no tile
    /// authored for this neighbourhood".
    tables: Vec<[u32; 256]>,
    kinds: Vec<AutotileKind>,
}

impl Autotiler {
    pub fn build(set: &TileSet) -> Self {
        let mut tables: Vec<[u32; 256]> = vec![[u32::MAX; 256]; set.groups.len()];
        let kinds: Vec<AutotileKind> = set.groups.iter().map(|g| g.kind).collect();

        // Assign each authored tile to every RAW mask that canonicalises to its
        // mask. A group therefore answers all 256 neighbourhoods as soon as its
        // canonical set is covered, and a half-authored group answers the ones
        // it can.
        for (&cell, info) in &set.tiles {
            let Some(g) = info.group else { continue };
            let (Some(table), Some(&kind)) = (tables.get_mut(g as usize), kinds.get(g as usize))
            else {
                continue;
            };
            let want = canonical(kind, info.mask);
            for raw in 0u16..=255 {
                if canonical(kind, raw as u8) == want {
                    // First tile wins for a mask. Two tiles claiming one mask is
                    // an authoring mistake the palette flags; picking the lower
                    // cell makes it at least deterministic, which matters because
                    // an autotiled level must come out the same on every machine.
                    let slot = &mut table[raw as usize];
                    if *slot == u32::MAX || cell < *slot {
                        *slot = cell;
                    }
                }
            }
        }
        Self { tables, kinds }
    }

    pub fn has_group(&self, group: u16) -> bool {
        (group as usize) < self.tables.len()
    }

    pub fn kind(&self, group: u16) -> Option<AutotileKind> {
        self.kinds.get(group as usize).copied()
    }

    /// The cell a group draws for a raw 8-neighbour mask, or `None` when the
    /// group has nothing authored for it.
    ///
    /// `None` means *leave the square alone* to every caller — never "erase it".
    /// A half-drawn autotile group should leave holes in the shape you painted,
    /// not delete the tiles that were already there.
    pub fn resolve(&self, group: u16, mask: u8) -> Option<u32> {
        let c = *self.tables.get(group as usize)?.get(mask as usize)?;
        (c != u32::MAX).then_some(c)
    }

    /// Which canonical masks a group has no tile for — what the palette shows as
    /// "12 of 47 drawn", and what makes an incomplete group visible rather than
    /// something you discover by finding a hole in a level.
    pub fn missing(&self, group: u16) -> Vec<u8> {
        let Some(kind) = self.kind(group) else { return Vec::new() };
        preset_masks(kind)
            .into_iter()
            .filter(|m| self.resolve(group, *m).is_none())
            .collect()
    }
}

/// Assign preset masks to a run of tiles, in cell order.
///
/// Returns `(cell, mask)` pairs to write. Extra tiles past the preset's length
/// get no mask (and the caller should say so) rather than wrapping round — a
/// wrapped mask would make two tiles claim one neighbourhood, and the loser
/// would simply never appear.
pub fn assign_preset(kind: AutotileKind, cells: &[u32]) -> Vec<(u32, u8)> {
    let masks = preset_masks(kind);
    cells.iter().copied().zip(masks).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tileset::{AutotileGroup, TileSet};

    #[test]
    fn the_bits_are_clockwise_from_north() {
        // The doc comment's diagram, as an assertion — so a renumbering has to
        // change the diagram too.
        assert_eq!(OFFSETS[0], (0, -1, N), "north is the LOWER row");
        let clockwise: Vec<u8> = OFFSETS.iter().map(|o| o.2).collect();
        assert_eq!(clockwise, vec![N, NE, E, SE, S, SW, W, NW]);
        // Each bit distinct, and the eight cover a byte.
        assert_eq!(clockwise.iter().fold(0u8, |a, b| a | b), 0xFF);
    }

    #[test]
    fn the_presets_are_the_sizes_they_claim() {
        assert_eq!(preset_masks(AutotileKind::Edge4).len(), 16);
        assert_eq!(preset_masks(AutotileKind::Blob8).len(), 47);
        assert_eq!(preset_masks(AutotileKind::Edge4).len(), preset_len(AutotileKind::Edge4));
        assert_eq!(preset_masks(AutotileKind::Blob8).len(), preset_len(AutotileKind::Blob8));
    }

    /// The 47 is not a magic number — it is what the corner rule leaves. If the
    /// rule ever changes, this test says so before an artist draws 47 tiles for
    /// a table that wants a different 47.
    #[test]
    fn the_corner_rule_is_what_turns_256_into_47() {
        let mut kept = 0;
        for raw in 0u16..=255 {
            if canonical(AutotileKind::Blob8, raw as u8) == raw as u8 {
                kept += 1;
            }
        }
        assert_eq!(kept, 47, "the canonical Blob8 masks are the fixed points of the rule");

        // A diagonal with no shared edge is invisible from inside the tile.
        assert_eq!(canonical(AutotileKind::Blob8, NE), 0);
        assert_eq!(canonical(AutotileKind::Blob8, NE | N), N);
        assert_eq!(canonical(AutotileKind::Blob8, NE | N | E), NE | N | E, "both edges: it counts");
    }

    #[test]
    fn edge4_ignores_the_corners_entirely() {
        for corner in [NE, SE, SW, NW] {
            assert_eq!(canonical(AutotileKind::Edge4, corner), 0);
            assert_eq!(canonical(AutotileKind::Edge4, N | corner), N);
        }
    }

    /// Canonicalising is idempotent — applying it twice is applying it once.
    /// A rule that was not would make a preset's own masks non-canonical, and
    /// tiles would answer neighbourhoods they were never assigned.
    #[test]
    fn canonicalising_twice_is_canonicalising_once() {
        for kind in AutotileKind::ALL {
            for raw in 0u16..=255 {
                let once = canonical(kind, raw as u8);
                assert_eq!(canonical(kind, once), once, "{kind:?} mask {raw:#b}");
            }
        }
    }

    fn grass_set(kind: AutotileKind, cells: &[u32]) -> TileSet {
        let mut set = TileSet { sheet_cols: 8, sheet_rows: 8, ..Default::default() };
        set.groups.push(AutotileGroup { name: "grass".into(), kind, joins: vec![] });
        for (cell, mask) in assign_preset(kind, cells) {
            let info = set.info_mut(cell);
            info.group = Some(0);
            info.mask = mask;
        }
        set
    }

    #[test]
    fn a_complete_group_answers_every_neighbourhood() {
        let cells: Vec<u32> = (0..16).collect();
        let set = grass_set(AutotileKind::Edge4, &cells);
        let at = Autotiler::build(&set);
        assert!(at.missing(0).is_empty(), "16 tiles is a complete Edge4 group");
        for raw in 0u16..=255 {
            assert!(at.resolve(0, raw as u8).is_some(), "no answer for {raw:#010b}");
        }
        // …and it answers CONSISTENTLY: two raw masks that canonicalise the same
        // must resolve to the same tile.
        for raw in 0u16..=255 {
            let a = at.resolve(0, raw as u8);
            let b = at.resolve(0, canonical(AutotileKind::Edge4, raw as u8));
            assert_eq!(a, b, "raw {raw:#010b} disagrees with its canonical form");
        }
    }

    /// A group whose sheet is half drawn answers what it can and says nothing for
    /// the rest — never a wrong tile, and never an erase.
    #[test]
    fn a_half_drawn_group_leaves_holes_rather_than_guessing() {
        let set = grass_set(AutotileKind::Blob8, &[0, 1, 2, 3]);
        let at = Autotiler::build(&set);
        let missing = at.missing(0);
        assert_eq!(missing.len(), 47 - 4, "43 neighbourhoods still undrawn");
        // The four that ARE drawn resolve.
        for m in preset_masks(AutotileKind::Blob8).into_iter().take(4) {
            assert!(at.resolve(0, m).is_some(), "mask {m:#010b} was assigned");
        }
        // A missing one answers None, not cell 0.
        let gap = *missing.last().unwrap();
        assert_eq!(at.resolve(0, gap), None);
    }

    #[test]
    fn two_tiles_claiming_one_mask_resolve_deterministically() {
        let mut set = grass_set(AutotileKind::Edge4, &(0..16).collect::<Vec<_>>());
        // Tile 20 also claims the mask tile 5 has.
        let mask = set.info(5).unwrap().mask;
        let info = set.info_mut(20);
        info.group = Some(0);
        info.mask = mask;
        let at = Autotiler::build(&set);
        assert_eq!(at.resolve(0, mask), Some(5), "the lower cell wins, every time");
    }

    #[test]
    fn an_unknown_group_resolves_to_nothing_rather_than_panicking() {
        let at = Autotiler::build(&TileSet::default());
        assert!(!at.has_group(0));
        assert_eq!(at.resolve(0, 0), None);
        assert_eq!(at.kind(7), None);
        assert!(at.missing(3).is_empty());
    }

    #[test]
    fn a_preset_longer_than_its_table_assigns_nothing_extra() {
        let cells: Vec<u32> = (0..30).collect();
        let pairs = assign_preset(AutotileKind::Edge4, &cells);
        assert_eq!(pairs.len(), 16, "the extra 14 tiles get no mask rather than a wrapped one");
        let masks: Vec<u8> = pairs.iter().map(|p| p.1).collect();
        assert_eq!(masks, preset_masks(AutotileKind::Edge4));
    }
}
