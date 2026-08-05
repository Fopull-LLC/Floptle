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

/// Mix three integers into a well-spread one.
///
/// The murmur3 finaliser, written out. It is here rather than behind a `Hasher`
/// because the numbers this produces are baked into levels: two squares must
/// pick the same variant on every machine, in every build, forever, and
/// `DefaultHasher` explicitly does not promise that.
fn spread(x: i32, y: i32, salt: u16) -> u64 {
    let mut h = ((x as u32 as u64) << 32) | (y as u32 as u64);
    h ^= (salt as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^ (h >> 33)
}

/// A resolver built once from a tileset, so painting a stroke does not re-scan
/// the group list per square.
///
/// Holds, per group, a 256-entry table from raw mask to a rule index. Raw rather
/// than canonical so a lookup is one index with no branching, and so a group
/// with an incomplete sheet still answers *something* for every neighbourhood
/// (see [`Autotiler::resolve`]).
pub struct Autotiler {
    /// `tables[group][mask]` = an index into `rules[group]`, or `u16::MAX` for
    /// "no tile authored for this neighbourhood".
    tables: Vec<[u16; 256]>,
    /// `rules[group][i]` = the tiles that rule draws, in authoring order.
    rules: Vec<Vec<Vec<u32>>>,
    kinds: Vec<AutotileKind>,
    /// Which groups draw a cell. The masking question — "does the tile next to
    /// me count as my stuff?" — is asked eight times per square of every
    /// retiled region, so it cannot be a scan of every group's rules.
    members: std::collections::HashMap<u32, Vec<u16>>,
}

impl Autotiler {
    pub fn build(set: &TileSet) -> Self {
        let mut tables: Vec<[u16; 256]> = vec![[u16::MAX; 256]; set.groups.len()];
        let mut rules: Vec<Vec<Vec<u32>>> = vec![Vec::new(); set.groups.len()];
        let kinds: Vec<AutotileKind> = set.groups.iter().map(|g| g.kind).collect();
        let mut members: std::collections::HashMap<u32, Vec<u16>> =
            std::collections::HashMap::new();

        for (gi, group) in set.groups.iter().enumerate() {
            let g = gi as u16;
            for rule in &group.rules {
                if rule.tiles.is_empty() {
                    continue;
                }
                let want = canonical(group.kind, rule.mask);
                // A hand-edited file could name one mask twice; the later rule's
                // tiles join the earlier one rather than replacing it, because
                // silently dropping authored art is the worse failure.
                let at = match tables[gi][want as usize] {
                    u16::MAX => {
                        rules[gi].push(Vec::new());
                        rules[gi].len() - 1
                    }
                    i => i as usize,
                };
                rules[gi][at].extend_from_slice(&rule.tiles);
                for &cell in &rule.tiles {
                    let list = members.entry(cell).or_default();
                    if !list.contains(&g) {
                        list.push(g);
                    }
                }
                // Every RAW mask that canonicalises to this one answers here, so
                // a group covers all 256 neighbourhoods as soon as its canonical
                // set is covered.
                for raw in 0u16..=255 {
                    if canonical(group.kind, raw as u8) == want {
                        tables[gi][raw as usize] = at as u16;
                    }
                }
            }
        }
        Self { tables, rules, kinds, members }
    }

    pub fn has_group(&self, group: u16) -> bool {
        (group as usize) < self.tables.len()
    }

    pub fn kind(&self, group: u16) -> Option<AutotileKind> {
        self.kinds.get(group as usize).copied()
    }

    /// Whether the tile in `cell` counts as `group`'s own stuff when masking:
    /// drawn by that group, or by one it joins.
    pub fn counts_as(&self, set: &TileSet, cell: u32, group: u16) -> bool {
        self.members
            .get(&cell)
            .is_some_and(|gs| gs.iter().any(|&other| set.joins(group, other)))
    }

    /// Every tile a group draws for a raw 8-neighbour mask, in authoring order.
    /// Empty when the group has nothing authored for it.
    pub fn variants(&self, group: u16, mask: u8) -> &[u32] {
        let Some(&i) = self.tables.get(group as usize).and_then(|t| t.get(mask as usize)) else {
            return &[];
        };
        if i == u16::MAX {
            return &[];
        }
        self.rules[group as usize].get(i as usize).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// The cell a group draws for a raw 8-neighbour mask, or `None` when the
    /// group has nothing authored for it.
    ///
    /// The FIRST variant — what the panel previews for a rule. A paint stroke
    /// wants [`Self::resolve_at`], which spreads the variants across the map.
    ///
    /// `None` means *leave the square alone* to every caller — never "erase it".
    /// A half-drawn autotile group should leave holes in the shape you painted,
    /// not delete the tiles that were already there.
    pub fn resolve(&self, group: u16, mask: u8) -> Option<u32> {
        self.variants(group, mask).first().copied()
    }

    /// The cell a group draws at a particular square.
    ///
    /// With one tile on the rule this is [`Self::resolve`]. With several it
    /// picks one from the square's own coordinates, so a field of grass varies
    /// and — the part that matters — varies the SAME way every time the map is
    /// retiled, on every machine. A variant chosen from a random number
    /// generator would reshuffle the level on every load.
    pub fn resolve_at(&self, group: u16, mask: u8, x: i32, y: i32) -> Option<u32> {
        let tiles = self.variants(group, mask);
        match tiles.len() {
            0 => None,
            1 => Some(tiles[0]),
            n => Some(tiles[(spread(x, y, group) % n as u64) as usize]),
        }
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
/// Returns `(cell, mask)` pairs to write.
///
/// **A selection that is a whole multiple of the preset's length is read as
/// variants.** Select 32 tiles for the 16-shape preset and each shape gets two,
/// which is exactly how a sheet with alternates is laid out — pass after pass in
/// the same order. Anything else assigns one tile per shape in order and stops
/// at the shorter of the two; the caller says what was left over, because a
/// preset that silently truncates is the failure shape this codebase keeps
/// paying for.
pub fn assign_preset(kind: AutotileKind, cells: &[u32]) -> Vec<(u32, u8)> {
    let masks = preset_masks(kind);
    let n = masks.len();
    if !cells.is_empty() && cells.len().is_multiple_of(n) {
        return cells.iter().copied().enumerate().map(|(i, c)| (c, masks[i % n])).collect();
    }
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
        set.groups.push(AutotileGroup { name: "grass".into(), kind, ..Default::default() });
        for (cell, mask) in assign_preset(kind, cells) {
            set.groups[0].add_to_rule(mask, cell);
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

    /// Two tiles on one rule are VARIANTS, not a conflict. This is the thing the
    /// old model could not express at all: a mask held one cell, so a second
    /// tile for the same shape silently replaced the first or was ignored.
    #[test]
    fn a_rule_with_several_tiles_spreads_them_across_the_map() {
        let mut set = grass_set(AutotileKind::Edge4, &(0..16).collect::<Vec<_>>());
        let mask = EDGES; // surrounded
        let base = set.groups[0].tiles_for(mask)[0];
        set.groups[0].add_to_rule(mask, 20);
        set.groups[0].add_to_rule(mask, 21);
        let at = Autotiler::build(&set);

        assert_eq!(at.variants(0, mask), &[base, 20, 21]);
        assert_eq!(at.resolve(0, mask), Some(base), "the preview is the first variant");

        // Over a field, every variant gets used and nothing else appears.
        let mut seen = std::collections::BTreeSet::new();
        for y in 0..40 {
            for x in 0..40 {
                seen.insert(at.resolve_at(0, mask, x, y).unwrap());
            }
        }
        assert_eq!(seen, [base, 20, 21].into_iter().collect(), "all three, and only those");
    }

    /// The variant a square gets must depend ONLY on where it is — asked twice,
    /// asked in another process, asked next year, the same square answers the
    /// same tile. A level that reshuffled itself on load would be unusable.
    #[test]
    fn a_squares_variant_is_the_same_every_time_it_is_asked() {
        let mut set = grass_set(AutotileKind::Edge4, &(0..16).collect::<Vec<_>>());
        for cell in [20, 21, 22] {
            set.groups[0].add_to_rule(EDGES, cell);
        }
        let at = Autotiler::build(&set);
        let once: Vec<u32> =
            (0..50).map(|i| at.resolve_at(0, EDGES, i, i * 3 - 7).unwrap()).collect();
        // A second resolver built from the same tileset, as a fresh load would.
        let again = Autotiler::build(&set);
        let twice: Vec<u32> =
            (0..50).map(|i| again.resolve_at(0, EDGES, i, i * 3 - 7).unwrap()).collect();
        assert_eq!(once, twice);
        // ...and neighbouring squares are not all the same one, which is the
        // whole point of having variants.
        assert!(once.windows(2).any(|w| w[0] != w[1]), "the spread is not spreading");
    }

    /// Listing a tile twice doubles its share. It is how an artist asks for a
    /// rare flower without drawing nine plain squares.
    #[test]
    fn a_tile_listed_twice_comes_up_twice_as_often() {
        let mut set = grass_set(AutotileKind::Edge4, &(0..16).collect::<Vec<_>>());
        set.groups[0].clear_rule(EDGES);
        for cell in [90, 90, 90, 91] {
            set.groups[0].add_to_rule(EDGES, cell);
        }
        let at = Autotiler::build(&set);
        let mut common = 0;
        let mut rare = 0;
        for y in 0..60 {
            for x in 0..60 {
                match at.resolve_at(0, EDGES, x, y) {
                    Some(90) => common += 1,
                    Some(91) => rare += 1,
                    other => panic!("unexpected {other:?}"),
                }
            }
        }
        assert!(common > rare * 2, "3:1 was authored, got {common}:{rare}");
    }

    /// The same tile in several rules — the other half of "duplicates". One
    /// plain fill square standing in for six neighbourhoods is ordinary.
    #[test]
    fn one_tile_can_answer_several_neighbourhoods() {
        let mut set = TileSet { sheet_cols: 8, sheet_rows: 8, ..Default::default() };
        set.groups.push(AutotileGroup {
            name: "grass".into(),
            kind: AutotileKind::Edge4,
            ..Default::default()
        });
        for mask in preset_masks(AutotileKind::Edge4) {
            set.groups[0].add_to_rule(mask, 4); // one tile, every shape
        }
        let at = Autotiler::build(&set);
        assert!(at.missing(0).is_empty(), "one tile covered the whole preset");
        for raw in 0u16..=255 {
            assert_eq!(at.resolve(0, raw as u8), Some(4));
        }
        assert_eq!(set.group_cells(0), vec![4], "and it is counted once, not sixteen times");
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
