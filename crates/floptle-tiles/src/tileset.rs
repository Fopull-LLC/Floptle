//! A **tileset**: what each cell of a spritesheet *means*, independent of where
//! it has been placed.
//!
//! ## Why this is a separate thing from the map
//!
//! The alternative is per-square data — a parallel "is this solid" grid beside
//! `data`. Every 2D tool tried that once and moved off it, for a reason worth
//! writing down: solidity is a property of the ART. A brick is solid everywhere
//! a brick appears. Storing it per square means the answer is recorded hundreds
//! of times, a level built before you decided bricks were solid keeps the old
//! answer, and marking one more tile solid is a job of repainting the level
//! rather than ticking a box.
//!
//! So the tileset is authored once, saved to `<project>/tilesets/<name>.tileset.ron`,
//! and referenced by every tilemap node cut from that sheet. Tick "solid" on the
//! brick and every brick in every scene collides — including the ones already
//! placed.
//!
//! ## What a tile can carry
//!
//! * **Collision** — none, the whole square, a half, or a hand-set rect
//!   ([`TileCollision`]). Rotating the tile rotates its collider with it.
//! * **Tags** — free strings a game reads (`tm:tagsAt(x, y)`): `"ice"`,
//!   `"water"`, `"damage"`. This is how a tilemap carries gameplay without the
//!   game keeping a second table keyed by cell index, which is the thing that
//!   goes stale when the artist reorders the sheet.
//! * **An autotile group + neighbour mask** — see [`crate::autotile`].
//! * **Animation frames** — a torch or a water surface is a list of cells and a
//!   rate, and every square using that tile animates together.
//!
//! Storage is sparse ([`std::collections::BTreeMap`]): a 256-tile sheet where
//! six tiles are solid stores six entries, and a tileset file stays readable by
//! a human. `BTreeMap` rather than a hash map so the `.ron` is written in cell
//! order and a diff of two tilesets is legible.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One side of a tile — which half a [`TileCollision::Half`] covers, named in
/// the tile's OWN art orientation (before any rotation).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TileSide {
    Top,
    Bottom,
    Left,
    Right,
}

impl TileSide {
    pub const ALL: [TileSide; 4] = [TileSide::Top, TileSide::Bottom, TileSide::Left, TileSide::Right];

    pub fn name(self) -> &'static str {
        match self {
            TileSide::Top => "top",
            TileSide::Bottom => "bottom",
            TileSide::Left => "left",
            TileSide::Right => "right",
        }
    }

    /// Every spelling accepted from Lua / a `.ron`, for the error message that
    /// lists them (`floptle/0082`: an enum parser and its accepted-values list
    /// must be the same code).
    pub const ACCEPTS: &'static [&'static str] = &["top", "bottom", "left", "right"];

    pub fn parse(s: &str) -> Option<TileSide> {
        match s.trim().to_ascii_lowercase().as_str() {
            "top" => Some(TileSide::Top),
            "bottom" => Some(TileSide::Bottom),
            "left" => Some(TileSide::Left),
            "right" => Some(TileSide::Right),
            _ => None,
        }
    }

    /// The half as a rect in the unit tile, `(x, y, w, h)` from the tile's
    /// BOTTOM-LEFT.
    fn rect(self) -> (f32, f32, f32, f32) {
        match self {
            TileSide::Bottom => (0.0, 0.0, 1.0, 0.5),
            TileSide::Top => (0.0, 0.5, 1.0, 0.5),
            TileSide::Left => (0.0, 0.0, 0.5, 1.0),
            TileSide::Right => (0.5, 0.0, 0.5, 1.0),
        }
    }
}

/// What a tile collides as.
///
/// Deliberately four cases and not a polygon editor. A slope needs real polygon
/// collision, and this engine's static colliders are boxes, spheres, capsules
/// and meshes — offering a slope here would mean drawing one and having it
/// behave as its bounding box, which is worse than not offering it. When
/// polygon tile collision lands, it lands as a fifth case.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum TileCollision {
    /// Walk through it. The default, so a fresh tileset collides with nothing
    /// and a level is not accidentally solid everywhere.
    #[default]
    None,
    /// The whole square. These are the ones that MERGE — a run of them becomes
    /// one box (see [`crate::collide`]).
    Full,
    /// Half the square, named in the art's own orientation. A rotated tile
    /// rotates its half with it, which is the entire reason the side is stored
    /// rather than a rect: "the top half" survives a quarter-turn, `y = 0.5`
    /// does not.
    Half(TileSide),
    /// A hand-set rect in the unit tile, from the BOTTOM-LEFT. For a ledge, a
    /// pipe, a fence post — the cases where the art is not half of anything.
    Custom { x: f32, y: f32, w: f32, h: f32 },
}

impl TileCollision {
    pub fn is_solid(self) -> bool {
        !matches!(self, TileCollision::None)
    }

    /// Whether this is the mergeable whole-square case.
    pub fn is_full(self) -> bool {
        matches!(self, TileCollision::Full)
    }

    /// The collider as a rect in the unit tile from the BOTTOM-LEFT, before the
    /// square's own orientation is applied. `None` for a non-collider.
    ///
    /// A `Custom` rect is clamped into the tile: a negative size or a rect that
    /// hangs outside would put a collider where no art is, and a tile whose
    /// collider is somewhere else entirely is the kind of bug that gets blamed
    /// on the physics engine.
    pub fn rect(self) -> Option<(f32, f32, f32, f32)> {
        match self {
            TileCollision::None => None,
            TileCollision::Full => Some((0.0, 0.0, 1.0, 1.0)),
            TileCollision::Half(side) => Some(side.rect()),
            TileCollision::Custom { x, y, w, h } => {
                let (x, y) = (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0));
                let (w, h) = (w.clamp(0.0, 1.0 - x), h.clamp(0.0, 1.0 - y));
                (w > 1e-4 && h > 1e-4).then_some((x, y, w, h))
            }
        }
    }

    pub fn label(self) -> String {
        match self {
            TileCollision::None => "none".into(),
            TileCollision::Full => "full".into(),
            TileCollision::Half(s) => format!("half {}", s.name()),
            TileCollision::Custom { x, y, w, h } => format!("rect {x:.2},{y:.2} {w:.2}x{h:.2}"),
        }
    }
}

/// Everything a tileset knows about one cell of the sheet.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TileInfo {
    pub collision: TileCollision,
    /// Gameplay tags a script reads. Order is authoring order.
    pub tags: Vec<String>,
    /// The autotile group this tile belongs to, as an index into
    /// [`TileSet::groups`]. `None` = an ordinary hand-placed tile.
    pub group: Option<u16>,
    /// The 8-neighbour bitmask this tile is the answer to, within its group.
    /// Meaningless without `group`. See [`crate::autotile::Neighbours`].
    pub mask: u8,
    /// Extra cells this tile cycles through, `anim_fps` per second. The tile's
    /// own index is frame 0 and is NOT repeated here, so a two-frame flicker is
    /// a one-entry list.
    pub frames: Vec<u32>,
    /// Frames per second. Zero (the default) means "do not animate", which is
    /// what a tile with no `frames` wants anyway.
    pub anim_fps: f32,
}

impl TileInfo {
    /// Whether this entry says anything at all. A tileset drops empty entries on
    /// save, so clearing a tile's last property removes it from the file rather
    /// than leaving a `TileInfo()` behind for every cell somebody ever clicked.
    pub fn is_blank(&self) -> bool {
        self.collision == TileCollision::None
            && self.tags.is_empty()
            && self.group.is_none()
            && self.frames.is_empty()
    }

    /// Which cell this tile shows at time `t` seconds.
    pub fn frame_at(&self, index: u32, t: f32) -> u32 {
        if self.frames.is_empty() || self.anim_fps <= 0.0 || !t.is_finite() {
            return index;
        }
        let n = self.frames.len() as u32 + 1;
        let step = ((t * self.anim_fps).floor().max(0.0) as u32) % n;
        if step == 0 { index } else { self.frames[(step - 1) as usize] }
    }
}

/// How an autotile group decides which of its tiles to draw.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutotileKind {
    /// Four neighbours (N/E/S/W) — 16 tiles. The cheap, universally understood
    /// one: a path, a wall run, a pipe network.
    #[default]
    Edge4,
    /// Eight neighbours with the corner rule — 47 tiles. What you need for
    /// terrain that has inside corners as well as outside ones.
    Blob8,
}

impl AutotileKind {
    pub const ALL: [AutotileKind; 2] = [AutotileKind::Edge4, AutotileKind::Blob8];

    pub fn name(self) -> &'static str {
        match self {
            AutotileKind::Edge4 => "edge4",
            AutotileKind::Blob8 => "blob8",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AutotileKind::Edge4 => "Edges (16 tiles)",
            AutotileKind::Blob8 => "Blob (47 tiles)",
        }
    }

    pub const ACCEPTS: &'static [&'static str] = &["edge4", "blob8"];

    pub fn parse(s: &str) -> Option<AutotileKind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "edge4" | "edge" | "16" => Some(AutotileKind::Edge4),
            "blob8" | "blob" | "47" => Some(AutotileKind::Blob8),
            _ => None,
        }
    }
}

/// A named set of tiles that pick themselves by what is next to them.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AutotileGroup {
    pub name: String,
    pub kind: AutotileKind,
    /// Other groups whose tiles count as "the same stuff" when masking.
    ///
    /// This is what lets a grass group and a dirt group meet without either
    /// growing an edge against the other: put each in the other's `joins` and a
    /// grass tile beside a dirt tile sees a filled neighbour. Not symmetric by
    /// construction — the editor sets both sides — because a one-way join is
    /// occasionally what you want (a cliff edges against sky but sky does not
    /// edge against the cliff).
    pub joins: Vec<u16>,
}

impl Default for AutotileGroup {
    fn default() -> Self {
        Self { name: "group".into(), kind: AutotileKind::Edge4, joins: Vec::new() }
    }
}

/// Per-tile data for one spritesheet.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TileSet {
    /// Display name. The file name is the identity; this is what a panel shows.
    pub name: String,
    /// The sheet this describes, project-relative. Informational: a tilemap
    /// node's own Material is still the authority for what is drawn, and the
    /// editor warns when the two disagree rather than silently overriding
    /// either. (Guessing would mean a tileset could repaint a node's art.)
    pub texture: String,
    pub sheet_cols: u32,
    pub sheet_rows: u32,
    /// The sheets AFTER the first. `texture`/`sheet_cols`/`sheet_rows` above are
    /// page 0; these are pages 1, 2, … in order (`floptle/0092`).
    ///
    /// Kept as a tail rather than folding page 0 into the list so a tileset
    /// written before pages existed loads with no migration and means exactly
    /// what it did — which is the same reason the first sheet keeps its own
    /// fields rather than being moved.
    #[serde(default)]
    pub pages: Vec<TilePage>,
    /// Sparse per-tile data, keyed by cell index. Only tiles that carry
    /// something are present.
    pub tiles: BTreeMap<u32, TileInfo>,
    pub groups: Vec<AutotileGroup>,
}

/// One sheet behind a tileset: an image and the uniform grid it is cut into.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TilePage {
    /// The image, project-relative.
    pub texture: String,
    pub cols: u32,
    pub rows: u32,
}

impl Default for TileSet {
    fn default() -> Self {
        Self {
            name: "tileset".into(),
            texture: String::new(),
            sheet_cols: 1,
            sheet_rows: 1,
            pages: Vec::new(),
            tiles: BTreeMap::new(),
            groups: Vec::new(),
        }
    }
}

impl TileSet {
    /// How many cells the FIRST sheet has.
    ///
    /// Page 0's count, not the tileset's total — the total is not a meaningful
    /// number under paging (the index space between two pages is a gap, not a
    /// run of cells), and every caller that wants "does this cell exist" wants
    /// [`Self::has_cell`] instead.
    pub fn cells(&self) -> u32 {
        self.sheet_cols.max(1) * self.sheet_rows.max(1)
    }

    /// How many sheets this tileset draws from. Always at least one.
    pub fn page_count(&self) -> u32 {
        1 + self.pages.len() as u32
    }

    /// A page's image and grid: `(texture, cols, rows)`.
    pub fn page(&self, page: u32) -> Option<(&str, u32, u32)> {
        if page == 0 {
            return Some((self.texture.as_str(), self.sheet_cols.max(1), self.sheet_rows.max(1)));
        }
        self.pages
            .get(page as usize - 1)
            .map(|p| (p.texture.as_str(), p.cols.max(1), p.rows.max(1)))
    }

    /// How many cells a page holds. `0` for a page this tileset does not have.
    pub fn page_cells(&self, page: u32) -> u32 {
        self.page(page).map(|(_, c, r)| c * r).unwrap_or(0)
    }

    /// Whether this tileset actually has the cell a square names.
    ///
    /// The paged replacement for `index < cells()`. Under paging that
    /// comparison is wrong in both directions: a page-1 cell is a large number
    /// and would read as past the end, while the gap between a page's real
    /// cells and its stride boundary would read as present and draw a sliver of
    /// whatever the UV maths landed on.
    pub fn has_cell(&self, cell: u32) -> bool {
        floptle_core::tile_in_page(cell) < self.page_cells(floptle_core::tile_page(cell))
    }

    /// Whether a packed square draws nothing: empty, or naming a cell no page of
    /// this tileset has. The one emptiness test for a tilemap that has a
    /// tileset — see [`Self::has_cell`].
    pub fn is_empty_square(&self, packed: u32) -> bool {
        packed == floptle_core::EMPTY_TILE || !self.has_cell(floptle_core::tile_index(packed))
    }

    /// Every page, with its cell range, in draw order.
    pub fn pages_iter(&self) -> impl Iterator<Item = (u32, &str, u32, u32)> + '_ {
        (0..self.page_count()).filter_map(move |p| {
            let (tex, c, r) = self.page(p)?;
            Some((p, tex, c, r))
        })
    }

    pub fn info(&self, cell: u32) -> Option<&TileInfo> {
        self.tiles.get(&cell)
    }

    /// The entry for `cell`, creating a blank one if needed. Callers that end up
    /// writing nothing should [`prune`](Self::prune).
    pub fn info_mut(&mut self, cell: u32) -> &mut TileInfo {
        self.tiles.entry(cell).or_default()
    }

    pub fn collision(&self, cell: u32) -> TileCollision {
        self.tiles.get(&cell).map(|t| t.collision).unwrap_or_default()
    }

    pub fn tags(&self, cell: u32) -> &[String] {
        self.tiles.get(&cell).map(|t| t.tags.as_slice()).unwrap_or(&[])
    }

    pub fn group_of(&self, cell: u32) -> Option<u16> {
        self.tiles.get(&cell)?.group
    }

    /// Whether the tileset animates at all — the editor asks this before it
    /// starts rebuilding tilemap meshes every frame.
    pub fn animated(&self) -> bool {
        self.tiles.values().any(|t| !t.frames.is_empty() && t.anim_fps > 0.0)
    }

    /// Drop entries that say nothing, so the file stays the size of what was
    /// actually authored.
    pub fn prune(&mut self) {
        self.tiles.retain(|_, t| !t.is_blank());
    }

    /// Every cell in a group, in cell order.
    pub fn group_cells(&self, group: u16) -> Vec<u32> {
        self.tiles
            .iter()
            .filter(|(_, t)| t.group == Some(group))
            .map(|(c, _)| *c)
            .collect()
    }

    /// Remove a group and un-assign its tiles, fixing up the indices of the
    /// groups after it (and anybody's `joins` that named them).
    ///
    /// Index fix-up is the whole content of this function. A group list where
    /// removing the first entry silently re-points every tile at its neighbour
    /// is the positional-id bug this codebase has already paid for twice
    /// (`floptle/0046`, and the builder scene's button ids).
    pub fn remove_group(&mut self, group: u16) {
        self.tiles.values_mut().for_each(|t| {
            t.group = match t.group {
                Some(g) if g == group => None,
                Some(g) if g > group => Some(g - 1),
                other => other,
            };
            if t.group.is_none() {
                t.mask = 0;
            }
        });
        if (group as usize) < self.groups.len() {
            self.groups.remove(group as usize);
        }
        for g in &mut self.groups {
            g.joins.retain(|j| *j != group);
            for j in &mut g.joins {
                if *j > group {
                    *j -= 1;
                }
            }
        }
    }

    /// Whether `other` counts as the same stuff as `group` for masking: itself,
    /// or something it joins.
    pub fn joins(&self, group: u16, other: u16) -> bool {
        group == other
            || self
                .groups
                .get(group as usize)
                .is_some_and(|g| g.joins.contains(&other))
    }

    pub fn to_ron(&self) -> Result<String, ron::Error> {
        ron::ser::to_string_pretty(self, ron::ser::PrettyConfig::new().struct_names(true))
    }

    pub fn from_ron(text: &str) -> Result<Self, ron::de::SpannedError> {
        ron::from_str(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_tileset_collides_with_nothing() {
        let set = TileSet::default();
        for cell in 0..16 {
            assert_eq!(set.collision(cell), TileCollision::None, "cell {cell}");
        }
    }

    #[test]
    fn a_custom_rect_is_clamped_into_its_own_tile() {
        // Hanging off the right edge: the width is cut, not the position moved —
        // a collider that slid left would be under the wrong art.
        let c = TileCollision::Custom { x: 0.75, y: 0.0, w: 0.9, h: 1.0 };
        assert_eq!(c.rect(), Some((0.75, 0.0, 0.25, 1.0)));
        // Degenerate rects are not colliders at all.
        assert_eq!(TileCollision::Custom { x: 0.5, y: 0.5, w: 0.0, h: 0.5 }.rect(), None);
        assert_eq!(TileCollision::Custom { x: 0.0, y: 0.0, w: -1.0, h: 1.0 }.rect(), None);
    }

    #[test]
    fn the_four_halves_are_the_halves_they_say() {
        assert_eq!(TileCollision::Half(TileSide::Bottom).rect(), Some((0.0, 0.0, 1.0, 0.5)));
        assert_eq!(TileCollision::Half(TileSide::Top).rect(), Some((0.0, 0.5, 1.0, 0.5)));
        assert_eq!(TileCollision::Half(TileSide::Left).rect(), Some((0.0, 0.0, 0.5, 1.0)));
        assert_eq!(TileCollision::Half(TileSide::Right).rect(), Some((0.5, 0.0, 0.5, 1.0)));
    }

    #[test]
    fn a_round_trip_through_ron_keeps_everything() {
        let mut set = TileSet { name: "bricks".into(), sheet_cols: 4, sheet_rows: 4, ..Default::default() };
        set.groups.push(AutotileGroup { name: "wall".into(), kind: AutotileKind::Blob8, joins: vec![] });
        set.info_mut(3).collision = TileCollision::Half(TileSide::Top);
        set.info_mut(3).tags = vec!["ice".into()];
        set.info_mut(5).collision = TileCollision::Full;
        set.info_mut(5).group = Some(0);
        set.info_mut(5).mask = 0b0101_0101;
        set.info_mut(7).frames = vec![8, 9];
        set.info_mut(7).anim_fps = 6.0;

        let text = set.to_ron().expect("serialize");
        let back = TileSet::from_ron(&text).expect("parse");
        assert_eq!(back.tiles, set.tiles);
        assert_eq!(back.groups.len(), 1);
        assert_eq!(back.groups[0].kind, AutotileKind::Blob8);
        assert_eq!(back.sheet_cols, 4);
    }

    /// An older tileset file is missing whatever was added since. `#[serde(default)]`
    /// on the struct is what makes that a load rather than an error, and this is
    /// the test that fails if somebody drops it.
    #[test]
    fn a_tileset_file_that_predates_a_field_still_loads() {
        let old = r#"TileSet(name: "old", sheet_cols: 8, sheet_rows: 8)"#;
        let set = TileSet::from_ron(old).expect("a partial tileset must load");
        assert_eq!(set.name, "old");
        assert_eq!(set.cells(), 64);
        assert!(set.tiles.is_empty());

        // …and so does a tile entry missing the newer per-tile fields.
        let old = r#"TileSet(name: "o", tiles: {2: TileInfo(collision: Full)})"#;
        let set = TileSet::from_ron(old).expect("a partial tile entry must load");
        assert_eq!(set.collision(2), TileCollision::Full);
        assert!(set.tags(2).is_empty());
    }

    #[test]
    fn removing_a_group_repoints_the_ones_after_it() {
        let mut set = TileSet::default();
        for name in ["grass", "dirt", "stone"] {
            set.groups.push(AutotileGroup { name: name.into(), ..Default::default() });
        }
        // stone joins grass; dirt joins stone.
        set.groups[2].joins = vec![0];
        set.groups[1].joins = vec![2];
        set.info_mut(0).group = Some(0);
        set.info_mut(1).group = Some(1);
        set.info_mut(2).group = Some(2);
        set.info_mut(2).mask = 0b1010;

        set.remove_group(0); // drop grass

        assert_eq!(set.groups.len(), 2);
        assert_eq!(set.groups[0].name, "dirt");
        assert_eq!(set.group_of(0), None, "a tile in the removed group is un-assigned");
        assert_eq!(set.info(0).unwrap().mask, 0, "…and loses its mask with it");
        assert_eq!(set.group_of(1), Some(0), "dirt slid down to 0");
        assert_eq!(set.group_of(2), Some(1), "stone slid down to 1");
        assert_eq!(set.groups[0].joins, vec![1], "dirt still joins stone at its new index");
        assert!(set.groups[1].joins.is_empty(), "stone's join on grass is gone, not re-pointed");
    }

    #[test]
    fn joins_are_reflexive_and_otherwise_only_what_was_said() {
        let mut set = TileSet::default();
        set.groups.push(AutotileGroup { name: "a".into(), joins: vec![1], ..Default::default() });
        set.groups.push(AutotileGroup { name: "b".into(), ..Default::default() });
        assert!(set.joins(0, 0));
        assert!(set.joins(0, 1));
        assert!(!set.joins(1, 0), "a one-way join stays one-way");
    }

    #[test]
    fn an_animated_tile_cycles_its_own_cell_first() {
        let mut info = TileInfo { frames: vec![9, 10], anim_fps: 4.0, ..Default::default() };
        // Frame 0 is the tile itself, then the listed frames, then round again.
        assert_eq!(info.frame_at(8, 0.0), 8);
        assert_eq!(info.frame_at(8, 0.26), 9);
        assert_eq!(info.frame_at(8, 0.51), 10);
        assert_eq!(info.frame_at(8, 0.76), 8);
        // No rate, no animation — however many frames are listed.
        info.anim_fps = 0.0;
        assert_eq!(info.frame_at(8, 5.0), 8);
        // And a NaN clock does not index out of bounds.
        info.anim_fps = 4.0;
        assert_eq!(info.frame_at(8, f32::NAN), 8);
    }

    #[test]
    fn pruning_drops_entries_that_say_nothing() {
        let mut set = TileSet::default();
        set.info_mut(1).collision = TileCollision::Full;
        set.info_mut(2); // touched and left blank
        set.info_mut(3).tags = vec!["x".into()];
        assert_eq!(set.tiles.len(), 3);
        set.prune();
        assert_eq!(set.tiles.keys().copied().collect::<Vec<_>>(), vec![1, 3]);
    }

    // ---- floptle/0092: more than one sheet behind one grid -------------------

    fn paged() -> TileSet {
        TileSet {
            texture: "ground.png".into(),
            sheet_cols: 4,
            sheet_rows: 4,
            pages: vec![
                TilePage { texture: "props.png".into(), cols: 2, rows: 3 },
                TilePage { texture: "deco.png".into(), cols: 8, rows: 8 },
            ],
            ..TileSet::default()
        }
    }

    /// The whole point of a fixed stride: adding art to page 0 must not change
    /// what any cell on a later page means, or a level saved yesterday draws
    /// garbage today.
    #[test]
    fn adding_art_to_one_sheet_renumbers_nothing_on_another() {
        let before = paged();
        let cell = floptle_core::tile_cell_of(1, 3);
        assert!(before.has_cell(cell));

        let mut after = before.clone();
        after.sheet_cols = 16; // the artist grew the first sheet
        after.sheet_rows = 16;
        assert_eq!(floptle_core::tile_page(cell), 1, "still page 1");
        assert_eq!(floptle_core::tile_in_page(cell), 3, "still the same cell of it");
        assert!(after.has_cell(cell));
    }

    /// A tileset written before pages existed means exactly what it did, and
    /// every index in it is page 0.
    #[test]
    fn a_tileset_from_before_pages_is_one_page() {
        let old: TileSet = ron::from_str(
            r#"(name: "old", texture: "t.png", sheet_cols: 4, sheet_rows: 4, tiles: {}, groups: [])"#,
        )
        .expect("an older tileset must still load");
        assert_eq!(old.page_count(), 1);
        assert_eq!(old.cells(), 16);
        assert!(old.pages.is_empty());
        for cell in 0..16 {
            assert!(old.has_cell(cell), "cell {cell} of the only sheet");
            assert_eq!(floptle_core::tile_page(cell), 0);
        }
        assert!(!old.has_cell(16), "and nothing past its end");
    }

    /// Emptiness is per PAGE. `index < cells()` is wrong in both directions
    /// under paging: a page-1 cell is a large number that would read as past the
    /// end, and the gap above a page's real cells would read as present.
    #[test]
    fn a_square_is_empty_when_no_page_has_its_cell() {
        let set = paged();
        assert!(set.has_cell(floptle_core::tile_cell_of(0, 15)));
        assert!(!set.has_cell(floptle_core::tile_cell_of(0, 16)));
        assert!(set.has_cell(floptle_core::tile_cell_of(1, 5)), "2x3 = 6 cells");
        assert!(!set.has_cell(floptle_core::tile_cell_of(1, 6)));
        assert!(set.has_cell(floptle_core::tile_cell_of(2, 63)));
        assert!(!set.has_cell(floptle_core::tile_cell_of(3, 0)), "there is no page 3");

        // ...and the packed-square test agrees, including for a rotated tile.
        let turned = floptle_core::tile_pack(
            floptle_core::tile_cell_of(1, 5),
            floptle_core::TileXform::new(2, true),
        );
        assert!(!set.is_empty_square(turned), "an orientation is not an emptiness");
        assert!(set.is_empty_square(floptle_core::EMPTY_TILE));
        assert!(set.is_empty_square(floptle_core::tile_cell_of(1, 6)));
    }

    /// Per-tile data is keyed by the global cell index, so it works the same on
    /// every page and two pages cannot collide over one entry.
    #[test]
    fn per_tile_data_works_on_every_page() {
        let mut set = paged();
        let a = floptle_core::tile_cell_of(0, 2);
        let b = floptle_core::tile_cell_of(2, 2);
        set.info_mut(a).collision = TileCollision::Full;
        set.info_mut(b).collision = TileCollision::Half(TileSide::Top);
        assert_eq!(set.collision(a), TileCollision::Full);
        assert_eq!(set.collision(b), TileCollision::Half(TileSide::Top));
        assert_eq!(set.collision(floptle_core::tile_cell_of(1, 2)), TileCollision::None);
    }

    /// The pages round-trip through the file, and a set with none writes what it
    /// always wrote.
    #[test]
    fn pages_round_trip_through_the_file() {
        let set = paged();
        let back: TileSet = ron::from_str(&ron::to_string(&set).unwrap()).unwrap();
        assert_eq!(back.pages, set.pages);
        assert_eq!(back.page(2).map(|(t, c, r)| (t.to_string(), c, r)), Some(("deco.png".into(), 8, 8)));
    }
}
