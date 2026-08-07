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

/// The collider a tile actually contributes, in the unit tile from the
/// BOTTOM-LEFT and before the square's own orientation is applied.
///
/// Every caller goes through [`TileCollision::shape`] rather than asking for a
/// rect, and that is deliberate: while there was a `rect()` accessor, a case it
/// could not express would have answered `None` — "this tile is not solid" —
/// and a slope you drew would have been walked through. An enum the compiler
/// makes you match on cannot fail that way.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TileShape<'a> {
    /// Walk through it.
    None,
    /// An axis-aligned rect `(x, y, w, h)`.
    Rect(f32, f32, f32, f32),
    /// A hand-drawn outline, in authoring order.
    Poly(&'a [[f32; 2]]),
}

/// What a tile collides as.
///
/// Four rect-shaped cases and a hand-drawn one. The rect cases came first
/// because this engine's static colliders were boxes, spheres, capsules and
/// meshes, and a slope that behaved as its bounding box would have been worse
/// than no slope at all. [`Poly`](TileCollision::Poly) is that fifth case, and
/// it is a real collider rather than a bounding box: the collision core is
/// signed-distance-first, so an extruded polygon is exact geometry there
/// (`floptle_physics::PolyPrismShape`) and not an approximation of one.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
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
    /// A hand-drawn outline in the unit tile, from the BOTTOM-LEFT, in
    /// authoring order. Three points or more; fewer is not a shape and is
    /// treated as no collider rather than as a degenerate one.
    ///
    /// This is what a **slope** is. The editor snaps each point to the sheet's
    /// pixel grid, so the diagonal you draw meets the next tile's diagonal
    /// exactly instead of a subpixel away — which is the difference between a
    /// ramp a character runs up and one it catches on every tile boundary.
    ///
    /// Concave is allowed. The distance field below is exact for either, so
    /// there is no reason to make an author think about it.
    Poly(Vec<[f32; 2]>),
}

impl TileCollision {
    pub fn is_solid(&self) -> bool {
        !matches!(self.shape(), TileShape::None)
    }

    /// Whether this is the mergeable whole-square case.
    pub fn is_full(&self) -> bool {
        matches!(self, TileCollision::Full)
    }

    /// The collider this tile contributes.
    ///
    /// A `Custom` rect is clamped into the tile and a `Poly` needs three points:
    /// a collider outside the art, or a shape that is not one, is the kind of
    /// bug that gets blamed on the physics engine.
    pub fn shape(&self) -> TileShape<'_> {
        match self {
            TileCollision::None => TileShape::None,
            TileCollision::Full => TileShape::Rect(0.0, 0.0, 1.0, 1.0),
            TileCollision::Half(side) => {
                let (x, y, w, h) = side.rect();
                TileShape::Rect(x, y, w, h)
            }
            TileCollision::Custom { x, y, w, h } => {
                let (x, y) = (x.clamp(0.0, 1.0), y.clamp(0.0, 1.0));
                let (w, h) = (w.clamp(0.0, 1.0 - x), h.clamp(0.0, 1.0 - y));
                if w > 1e-4 && h > 1e-4 {
                    TileShape::Rect(x, y, w, h)
                } else {
                    TileShape::None
                }
            }
            TileCollision::Poly(pts) => {
                if pts.len() >= 3 && polygon_area(pts).abs() > 1e-6 {
                    TileShape::Poly(pts)
                } else {
                    TileShape::None
                }
            }
        }
    }

    pub fn label(&self) -> String {
        match self {
            TileCollision::None => "none".into(),
            TileCollision::Full => "full".into(),
            TileCollision::Half(s) => format!("half {}", s.name()),
            TileCollision::Custom { x, y, w, h } => format!("rect {x:.2},{y:.2} {w:.2}x{h:.2}"),
            TileCollision::Poly(p) => format!("shape, {} points", p.len()),
        }
    }
}

/// Twice the signed area of a polygon — the shoelace sum. Zero means the points
/// are collinear or doubled back on themselves, which is a line and not a
/// collider.
pub fn polygon_area(pts: &[[f32; 2]]) -> f32 {
    let mut a = 0.0;
    for i in 0..pts.len() {
        let p = pts[i];
        let q = pts[(i + 1) % pts.len()];
        a += p[0] * q[1] - q[0] * p[1];
    }
    a * 0.5
}

fn is_zero(m: &u8) -> bool {
    *m == 0
}

/// Everything a tileset knows about one cell of the sheet.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TileInfo {
    pub collision: TileCollision,
    /// Gameplay tags a script reads. Order is authoring order.
    pub tags: Vec<String>,
    /// **Legacy.** The autotile group this tile belonged to, back when the
    /// mapping lived on the tile.
    ///
    /// A tile could name one group and one mask, which meant one tile could
    /// answer exactly one neighbourhood and no neighbourhood could offer a
    /// choice of tiles. Both are things an artist asks for constantly — a plain
    /// fill that serves several shapes, four grass tiles that vary — so the
    /// mapping now lives on the group as [`AutotileGroup::rules`].
    ///
    /// Kept only so a tileset written before that loads: it is read once by
    /// [`TileSet::migrate_legacy_autotile`], moved into the group's rules, and
    /// cleared. Never written back out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<u16>,
    /// **Legacy.** The neighbourhood `group` said this tile answered. See
    /// `group`.
    #[serde(default, skip_serializing_if = "is_zero")]
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
            && self.frames.is_empty()
            // Only true between parsing an old file and migrating it — after
            // that these are always clear. Kept so a prune in that window
            // cannot drop the assignment before it has been converted.
            && self.group.is_none()
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

/// One rule of an autotile group: a neighbourhood, and the tiles that draw it.
///
/// `tiles` is a LIST, and both directions of that matter to somebody drawing a
/// tileset:
///
/// * **The same tile may appear in any number of rules.** A plain fill square
///   is usually the answer to several neighbourhoods, and a sheet where the
///   artist drew one inside-corner rather than four wants that one tile in four
///   rules.
/// * **A rule may hold any number of tiles.** They are *variants*: four grass
///   tiles that all mean "surrounded", picked per square so a field is not one
///   image repeated. Listing a tile twice makes it twice as likely, which is how
///   you get a rare flower without drawing nine plain squares.
///
/// The choice is made from the square's own position ([`Autotiler::resolve_at`])
/// and never from a random number generator, so the same map comes out the same
/// on every machine and in every session — a level that reshuffled itself on
/// load would be unusable.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutotileRule {
    /// The neighbourhood this answers, canonical for the group's kind.
    pub mask: u8,
    /// The tiles that draw it, in authoring order.
    ///
    /// Never empty in a rule this code keeps: emptying one drops the rule, and
    /// the two mean the same thing anyway — a neighbourhood with no tile leaves
    /// its square exactly as painted rather than erasing it. A hand-written file
    /// may still carry an empty rule, and it resolves to nothing.
    pub tiles: Vec<u32>,
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
    /// Which tile to draw for which neighbourhood. At most one rule per mask;
    /// order is the preset's ascending mask order.
    pub rules: Vec<AutotileRule>,
}

impl Default for AutotileGroup {
    fn default() -> Self {
        Self {
            name: "group".into(),
            kind: AutotileKind::Edge4,
            joins: Vec::new(),
            rules: Vec::new(),
        }
    }
}

impl AutotileGroup {
    /// The tiles this group draws for `mask`, in authoring order. The mask is
    /// canonicalised first, so asking with a raw neighbourhood works.
    pub fn tiles_for(&self, mask: u8) -> &[u32] {
        let want = crate::autotile::canonical(self.kind, mask);
        self.rules
            .iter()
            .find(|r| r.mask == want)
            .map(|r| r.tiles.as_slice())
            .unwrap_or(&[])
    }

    /// The rule for `mask`, created empty if this group has none yet.
    pub fn rule_mut(&mut self, mask: u8) -> &mut AutotileRule {
        let want = crate::autotile::canonical(self.kind, mask);
        let at = match self.rules.iter().position(|r| r.mask == want) {
            Some(i) => i,
            None => {
                // Keep the list in ascending mask order — it is the order the
                // preset states and the order the panel draws, and a file whose
                // rules are in click order would diff against itself.
                let i = self.rules.iter().take_while(|r| r.mask < want).count();
                self.rules.insert(i, AutotileRule { mask: want, tiles: Vec::new() });
                i
            }
        };
        &mut self.rules[at]
    }

    /// Add a tile to a rule. The same tile twice is allowed and meaningful —
    /// it doubles that tile's share of the variants.
    pub fn add_to_rule(&mut self, mask: u8, cell: u32) {
        self.rule_mut(mask).tiles.push(cell);
    }

    /// Drop ONE occurrence of `cell` from a rule, at `nth` among its variants.
    /// Removing by position rather than by value is what lets a duplicate be
    /// removed once without taking its twin with it.
    pub fn remove_variant(&mut self, mask: u8, nth: usize) {
        let want = crate::autotile::canonical(self.kind, mask);
        if let Some(r) = self.rules.iter_mut().find(|r| r.mask == want)
            && nth < r.tiles.len()
        {
            r.tiles.remove(nth);
        }
        self.rules.retain(|r| !r.tiles.is_empty());
    }

    pub fn clear_rule(&mut self, mask: u8) {
        let want = crate::autotile::canonical(self.kind, mask);
        self.rules.retain(|r| r.mask != want);
    }

    /// Every tile this group draws, deduplicated, in cell order.
    pub fn cells(&self) -> Vec<u32> {
        let mut out: Vec<u32> = self.rules.iter().flat_map(|r| r.tiles.iter().copied()).collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Whether this group draws `cell` anywhere.
    pub fn draws(&self, cell: u32) -> bool {
        self.rules.iter().any(|r| r.tiles.contains(&cell))
    }
}

/// Per-tile data for one spritesheet.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TileSet {
    /// Display name. The file name is the identity; this is what a panel shows.
    pub name: String,
    /// The sheet this describes, project-relative — and **the sheet a tilemap
    /// using this tileset draws**.
    ///
    /// This was once informational, on the reasoning that letting a tileset
    /// repaint a node's art would be worse than making the node's Material the
    /// authority. What that cost was the feature: a tileset describing a sheet
    /// it does not draw means every tilemap needs a Material carrying the same
    /// image and the same cut, kept in agreement by hand, and a tileset on its
    /// own renders nothing.
    ///
    /// Empty falls back to the node's Material, which is every tileset written
    /// before this — so nothing is repainted that did not ask to be.
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

    /// Borrowed rather than returned by value: a hand-drawn outline carries its
    /// points, and the collider builder asks this once per square of a level.
    pub fn collision(&self, cell: u32) -> &TileCollision {
        static NONE: TileCollision = TileCollision::None;
        self.tiles.get(&cell).map(|t| &t.collision).unwrap_or(&NONE)
    }

    pub fn tags(&self, cell: u32) -> &[String] {
        self.tiles.get(&cell).map(|t| t.tags.as_slice()).unwrap_or(&[])
    }

    /// The first group that draws `cell`, for the panel's "clicking this tile
    /// arms its autotile".
    ///
    /// A tile can now be drawn by more than one group, so this is a UI
    /// convenience and NOT what masking asks — masking wants every group the
    /// tile belongs to, which is [`Self::groups_of`] (and, in the inner loop,
    /// [`crate::Autotiler::counts_as`]).
    pub fn group_of(&self, cell: u32) -> Option<u16> {
        self.groups_of(cell).next()
    }

    /// Every group that draws `cell`, ascending.
    pub fn groups_of(&self, cell: u32) -> impl Iterator<Item = u16> + '_ {
        self.groups
            .iter()
            .enumerate()
            .filter(move |(_, g)| g.draws(cell))
            .map(|(i, _)| i as u16)
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

    /// Every cell a group draws, deduplicated, in cell order.
    pub fn group_cells(&self, group: u16) -> Vec<u32> {
        self.groups.get(group as usize).map(|g| g.cells()).unwrap_or_default()
    }

    /// Move a tileset written before rules lived on the group into the shape
    /// this code reads. Idempotent, and a no-op for anything already migrated.
    ///
    /// Runs on load ([`Self::from_ron`]), so the rest of the engine never sees
    /// the old shape. Only groups with NO rules are migrated — a group that has
    /// been authored since is left exactly alone, so re-reading a half-converted
    /// project cannot undo work.
    ///
    /// Cell order is preserved as variant order, which makes the conversion
    /// behaviour-identical: the old resolver broke a tie between two tiles
    /// claiming one mask by taking the lower cell, and the lower cell is still
    /// what a single-variant lookup returns.
    pub fn migrate_legacy_autotile(&mut self) {
        let legacy: Vec<(u32, u16, u8)> = self
            .tiles
            .iter()
            .filter_map(|(&cell, t)| t.group.map(|g| (cell, g, t.mask)))
            .collect();
        if legacy.is_empty() {
            return;
        }
        // Decided BEFORE anything is written: a group that already has rules is
        // authored, and folding the stale per-tile masks into it would resurrect
        // assignments somebody deliberately changed.
        let convert: Vec<bool> = self.groups.iter().map(|g| g.rules.is_empty()).collect();
        for (cell, g, mask) in legacy {
            if convert.get(g as usize) != Some(&true) {
                continue;
            }
            if let Some(group) = self.groups.get_mut(g as usize) {
                group.add_to_rule(mask, cell);
            }
        }
        for t in self.tiles.values_mut() {
            t.group = None;
            t.mask = 0;
        }
        self.prune();
    }

    /// Remove a group, fixing up the indices of the groups after it (and
    /// anybody's `joins` that named them).
    ///
    /// Index fix-up is the whole content of this function. A group list where
    /// removing the first entry silently re-points every tile at its neighbour
    /// is the positional-id bug this codebase has already paid for twice
    /// (`floptle/0046`, and the builder scene's button ids).
    pub fn remove_group(&mut self, group: u16) {
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

    /// Parse a tileset file, converting anything written before autotile rules
    /// moved onto the group. This is the ONE parse point, so no other code has
    /// to know the old shape existed.
    pub fn from_ron(text: &str) -> Result<Self, ron::de::SpannedError> {
        let mut set: Self = ron::from_str(text)?;
        set.migrate_legacy_autotile();
        Ok(set)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_tileset_collides_with_nothing() {
        let set = TileSet::default();
        for cell in 0..16 {
            assert_eq!(*set.collision(cell), TileCollision::None, "cell {cell}");
        }
    }

    #[test]
    fn a_custom_rect_is_clamped_into_its_own_tile() {
        // Hanging off the right edge: the width is cut, not the position moved —
        // a collider that slid left would be under the wrong art.
        let c = TileCollision::Custom { x: 0.75, y: 0.0, w: 0.9, h: 1.0 };
        assert_eq!(c.shape(), TileShape::Rect(0.75, 0.0, 0.25, 1.0));
        // Degenerate rects are not colliders at all.
        assert_eq!(TileCollision::Custom { x: 0.5, y: 0.5, w: 0.0, h: 0.5 }.shape(), TileShape::None);
        assert_eq!(TileCollision::Custom { x: 0.0, y: 0.0, w: -1.0, h: 1.0 }.shape(), TileShape::None);
    }

    #[test]
    fn the_four_halves_are_the_halves_they_say() {
        assert_eq!(TileCollision::Half(TileSide::Bottom).shape(), TileShape::Rect(0.0, 0.0, 1.0, 0.5));
        assert_eq!(TileCollision::Half(TileSide::Top).shape(), TileShape::Rect(0.0, 0.5, 1.0, 0.5));
        assert_eq!(TileCollision::Half(TileSide::Left).shape(), TileShape::Rect(0.0, 0.0, 0.5, 1.0));
        assert_eq!(TileCollision::Half(TileSide::Right).shape(), TileShape::Rect(0.5, 0.0, 0.5, 1.0));
    }

    #[test]
    fn a_round_trip_through_ron_keeps_everything() {
        let mut set = TileSet { name: "bricks".into(), sheet_cols: 4, sheet_rows: 4, ..Default::default() };
        set.groups.push(AutotileGroup { name: "wall".into(), kind: AutotileKind::Blob8, ..Default::default() });
        set.info_mut(3).collision = TileCollision::Half(TileSide::Top);
        set.info_mut(3).tags = vec!["ice".into()];
        set.info_mut(5).collision = TileCollision::Full;
        set.groups[0].add_to_rule(0b0101_0101, 5);
        set.groups[0].add_to_rule(0b0101_0101, 6); // a second variant of one rule
        set.info_mut(7).frames = vec![8, 9];
        set.info_mut(7).anim_fps = 6.0;

        let text = set.to_ron().expect("serialize");
        let back = TileSet::from_ron(&text).expect("parse");
        assert_eq!(back.tiles, set.tiles);
        assert_eq!(back.groups.len(), 1);
        assert_eq!(back.groups[0].kind, AutotileKind::Blob8);
        assert_eq!(back.groups[0].rules, set.groups[0].rules, "the rules survive the file");
        assert_eq!(back.groups[0].tiles_for(0b0101_0101), &[5, 6]);
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
        assert_eq!(*set.collision(2), TileCollision::Full);
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
        set.groups[0].add_to_rule(0, 0);
        set.groups[1].add_to_rule(0, 1);
        set.groups[2].add_to_rule(0b1010, 2);

        set.remove_group(0); // drop grass

        assert_eq!(set.groups.len(), 2);
        assert_eq!(set.groups[0].name, "dirt");
        assert_eq!(set.group_of(0), None, "the removed group's rules went with it");
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

    // ---- rules moved from the tile to the group ------------------------------

    /// The tileset every project on disk today is written in: the group/mask
    /// pair sat on each tile. It has to come back as rules, drawing the same
    /// tiles for the same shapes, or every level made so far retiles wrong.
    #[test]
    fn a_tileset_that_kept_its_autotile_on_the_tiles_is_converted() {
        let old = r#"TileSet(
            name: "cave",
            texture: "cave.png",
            sheet_cols: 8,
            sheet_rows: 8,
            tiles: {
                0: TileInfo(collision: Full, group: Some(0), mask: 0),
                1: TileInfo(collision: Full, group: Some(0), mask: 1),
                2: TileInfo(group: Some(0), mask: 5),
                9: TileInfo(tags: ["ice"]),
            },
            groups: [(name: "wall", kind: Edge4, joins: [])],
        )"#;
        let set = TileSet::from_ron(old).expect("an older tileset must load");

        assert_eq!(set.groups[0].tiles_for(0), &[0]);
        assert_eq!(set.groups[0].tiles_for(1), &[1]);
        assert_eq!(set.groups[0].tiles_for(5), &[2]);
        assert_eq!(set.group_cells(0), vec![0, 1, 2]);
        // Everything that was NOT autotile data is untouched.
        assert_eq!(*set.collision(0), TileCollision::Full);
        assert_eq!(set.tags(9), ["ice"]);

        // The legacy fields are cleared, so the file it saves back is in the new
        // shape and this conversion happens exactly once.
        assert!(set.tiles.values().all(|t| t.group.is_none() && t.mask == 0));
        let text = set.to_ron().expect("serialize");
        assert!(!text.contains("group:"), "the legacy field is not written back:\n{text}");
        assert!(text.contains("rules:"));
        // ...and a second pass over the saved file changes nothing.
        let again = TileSet::from_ron(&text).expect("reload");
        assert_eq!(again.groups[0].rules, set.groups[0].rules);
    }

    /// Tiles that carried an autotile assignment and nothing else must not be
    /// pruned out from under the conversion.
    #[test]
    fn a_tile_that_only_ever_had_an_autotile_assignment_survives_the_move() {
        let old = r#"TileSet(name: "t", sheet_cols: 4, sheet_rows: 4,
            tiles: {3: TileInfo(group: Some(0), mask: 4)},
            groups: [(name: "g", kind: Edge4, joins: [])])"#;
        let set = TileSet::from_ron(old).expect("load");
        assert_eq!(set.groups[0].tiles_for(4), &[3]);
        assert!(set.tiles.is_empty(), "the entry itself had nothing left to say");
    }

    /// A group somebody has authored rules for is NOT re-converted — the stale
    /// per-tile masks would resurrect assignments that were deliberately moved.
    #[test]
    fn a_group_that_already_has_rules_is_left_alone() {
        let mixed = r#"TileSet(name: "t", sheet_cols: 4, sheet_rows: 4,
            tiles: {3: TileInfo(group: Some(0), mask: 4), 7: TileInfo(group: Some(1), mask: 1)},
            groups: [
                (name: "authored", kind: Edge4, joins: [], rules: [(mask: 4, tiles: [11, 12])]),
                (name: "old", kind: Edge4, joins: []),
            ])"#;
        let set = TileSet::from_ron(mixed).expect("load");
        assert_eq!(set.groups[0].tiles_for(4), &[11, 12], "tile 3 was not folded back in");
        assert_eq!(set.groups[1].tiles_for(1), &[7], "the untouched group still converts");
    }

    #[test]
    fn a_rule_keeps_its_masks_in_order_however_they_are_added() {
        let mut g = AutotileGroup { kind: AutotileKind::Edge4, ..Default::default() };
        for mask in [20, 4, 17, 0] {
            g.add_to_rule(mask, mask as u32);
        }
        let masks: Vec<u8> = g.rules.iter().map(|r| r.mask).collect();
        assert_eq!(masks, vec![0, 4, 17, 20], "ascending, so the file diffs against itself");
    }

    /// Removing one variant of a duplicated tile must not take its twin.
    #[test]
    fn removing_a_variant_removes_one_of_them() {
        let mut g = AutotileGroup { kind: AutotileKind::Edge4, ..Default::default() };
        for cell in [7, 9, 7] {
            g.add_to_rule(4, cell);
        }
        g.remove_variant(4, 0);
        assert_eq!(g.tiles_for(4), &[9, 7]);
        g.remove_variant(4, 1);
        assert_eq!(g.tiles_for(4), &[9]);
        // Emptying a rule drops it, so "how many shapes are drawn" stays honest.
        g.remove_variant(4, 0);
        assert!(g.rules.is_empty());
        // And an index past the end is a no-op, not a panic.
        g.remove_variant(4, 3);
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
        assert_eq!(*set.collision(a), TileCollision::Full);
        assert_eq!(*set.collision(b), TileCollision::Half(TileSide::Top));
        assert_eq!(*set.collision(floptle_core::tile_cell_of(1, 2)), TileCollision::None);
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
