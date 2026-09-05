//! The ▦ Tiles suite's editor-side state: the project's tilesets, the tool the
//! pointer is holding, and what a click does to a tilemap node.
//!
//! The rules live in [`floptle_tiles`] — this is the part that knows about
//! files, entities, undo and the cursor. Split that way because "does the bucket
//! fill leak through a diagonal" is a kernel test and "does clicking there paint
//! the square under the pointer" is not.
//!
//! ## Layers are nodes
//!
//! There is no layer list here. A tilemap layer is a `Matter::Tilemap` NODE: it
//! already has a transform (so Z orders it), a Material (so each layer has its
//! own sheet), a `Visible` flag, a name and a place in the Hierarchy. The tab's
//! layer list is a view of the scene's tilemap nodes, and hiding a layer is the
//! ordinary node operation — which means the Hierarchy and the Tiles tab can
//! never disagree about whether a layer is showing.
//!
//! ## Undo is scene undo
//!
//! A tilemap's squares live in its own `Matter::Tilemap` component, which is
//! scene state. So a paint stroke is `begin_edit()` + writes, and Ctrl-Z is the
//! same Ctrl-Z as everything else — no private tile history to keep in step with
//! the scene's, which is what the terrain and vertex-paint stores had to do
//! (their data lives outside the scene) and what they pay for it.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use floptle_core::math::Vec2;
use floptle_core::{Entity, Matter, TileXform};
use floptle_tiles::{Autotiler, Stamp, TileGrid, TileSet};

use crate::Editor;

/// The project's tilesets, and which of them have unsaved edits.
#[derive(Default)]
pub(crate) struct TileStore {
    /// Project-relative path → the parsed tileset.
    pub(crate) sets: HashMap<String, TileSet>,
    /// Paths whose file exists but could NOT be parsed.
    ///
    /// While a path is in here the store is NOT the authority for it: it is not
    /// healed into a blank tileset and it is never saved over. A blank tileset
    /// written over a parse failure would silently un-solid an entire level and
    /// erase every autotile group — the same failure mode that ate a night of
    /// vertex paint in July, so the same guard.
    pub(crate) load_failed: HashSet<String>,
    /// Paths with edits not yet written to disk.
    pub(crate) dirty: BTreeSet<String>,
}

impl TileStore {
    pub(crate) fn get(&self, path: &str) -> Option<&TileSet> {
        self.sets.get(path)
    }

    pub(crate) fn get_mut(&mut self, path: &str) -> Option<&mut TileSet> {
        self.dirty.insert(path.to_string());
        self.sets.get_mut(path)
    }

    /// Every tileset path, sorted — what the tab's dropdown lists.
    pub(crate) fn paths(&self) -> Vec<String> {
        let mut v: Vec<String> = self.sets.keys().cloned().collect();
        v.sort();
        v
    }
}

/// Which tile tool the pointer is holding.
///
/// One list, in keybind order, read by the toolbar and by the shortcut handler —
/// so a tool cannot appear in one and not the other (the mistake `Tool::ALL`
/// exists to prevent for the main toolbar).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TileTool {
    /// Paint the stamp under the pointer, dragging to paint a stroke.
    #[default]
    Brush,
    /// Clear squares.
    Erase,
    /// Drag a filled rectangle.
    Rect,
    /// Drag a rectangle outline — a room's walls in one gesture.
    Frame,
    /// Drag a straight line.
    Line,
    /// Flood-fill the connected region under the pointer.
    Bucket,
    /// Take the square under the pointer as the current stamp (eyedropper).
    Pick,
    /// Drag out a rectangle of squares to operate on.
    Select,
    /// Drag the selected rectangle somewhere else.
    Move,
}

impl TileTool {
    pub(crate) const ALL: [TileTool; 9] = [
        TileTool::Brush,
        TileTool::Erase,
        TileTool::Rect,
        TileTool::Frame,
        TileTool::Line,
        TileTool::Bucket,
        TileTool::Pick,
        TileTool::Select,
        TileTool::Move,
    ];

    pub(crate) fn glyph(self) -> &'static str {
        match self {
            TileTool::Brush => "✏",
            TileTool::Erase => "✖",
            TileTool::Rect => "▬",
            TileTool::Frame => "▭",
            TileTool::Line => "╱",
            TileTool::Bucket => "◍",
            TileTool::Pick => "◉",
            TileTool::Select => "▢",
            TileTool::Move => "✚",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            TileTool::Brush => "Brush",
            TileTool::Erase => "Erase",
            TileTool::Rect => "Rectangle",
            TileTool::Frame => "Frame",
            TileTool::Line => "Line",
            TileTool::Bucket => "Fill",
            TileTool::Pick => "Pick",
            TileTool::Select => "Select",
            TileTool::Move => "Move",
        }
    }

    /// The single-key shortcut, lowercase.
    pub(crate) fn key(self) -> char {
        match self {
            TileTool::Brush => 'b',
            TileTool::Erase => 'e',
            TileTool::Rect => 'r',
            TileTool::Frame => 'f',
            TileTool::Line => 'l',
            TileTool::Bucket => 'g', // g for "fill", as in every paint program
            TileTool::Pick => 'i',   // i for eyedropper, as in every paint program
            TileTool::Select => 's',
            TileTool::Move => 'm',
        }
    }

    pub(crate) fn hint(self) -> &'static str {
        match self {
            TileTool::Brush => "paint the current tile; drag for a stroke (B)",
            TileTool::Erase => "clear squares; drag to erase a stroke (E)",
            TileTool::Rect => "drag a filled rectangle (R)",
            TileTool::Frame => "drag a rectangle outline — a room's walls in one go (F)",
            TileTool::Line => "drag a straight line (L)",
            TileTool::Bucket => "fill the connected region under the pointer (G)",
            TileTool::Pick => "take the square under the pointer as the current tile (I)",
            TileTool::Select => "drag out a rectangle to copy, move, turn or clear (S)",
            TileTool::Move => "drag the selection somewhere else (M)",
        }
    }

    /// Whether holding the button should keep painting as the pointer moves.
    fn is_stroke(self) -> bool {
        matches!(self, TileTool::Brush | TileTool::Erase)
    }

    /// Whether the tool works between a press and a release (a rubber band).
    fn is_drag(self) -> bool {
        matches!(
            self,
            TileTool::Rect | TileTool::Frame | TileTool::Line | TileTool::Select | TileTool::Move
        )
    }
}

/// What the ▦ Tiles tab and the viewport share.
pub(crate) struct TileTools {
    pub(crate) tool: TileTool,
    /// The tilemap node being painted. `None` = nothing selected yet.
    pub(crate) layer: Option<Entity>,
    /// What the brush paints: one square, or a rectangle lifted from the palette
    /// or from the map.
    pub(crate) stamp: Stamp,
    /// The stamp's orientation, as the ⇔ / ⇕ / ↻ buttons compose it.
    pub(crate) xform: TileXform,
    /// The palette's rubber-band selection, as `(px, py, w, h)` in sheet cells.
    pub(crate) palette: Option<(u32, u32, u32, u32)>,
    /// Which SHEET of the tileset the palette is showing (`floptle/0092`).
    ///
    /// 0 is the layer's own material sheet — which is every project that has
    /// never added a page, so this defaulting to 0 is the whole of the backward
    /// compatibility on the editing side.
    pub(crate) page: u32,
    /// Paint an autotile GROUP rather than a literal tile: the tile placed is
    /// whichever of the group's tiles fits its neighbours.
    pub(crate) group: Option<u16>,
    /// Retile after every stroke. On by default when a group is armed, because a
    /// group you have to remember to retile is a group that looks broken.
    pub(crate) auto_retile: bool,
    /// The autotile rule being filled: `(group, neighbourhood mask)`.
    ///
    /// While this is armed, clicking a tile in the palette does not pick a brush
    /// — it says "*this* is the tile for that neighbourhood" and moves to the
    /// next unfilled one. That is the whole of the interactive setup: the rules
    /// used to be assigned in bulk, in cell order, from a multi-selection, which
    /// works only for a sheet already laid out in the preset's order and gives
    /// no clue what went where when it isn't.
    pub(crate) fill_mask: Option<(u16, u8)>,
    /// After a rule is given its first tile, arm the next empty one.
    ///
    /// On, because filling a 47-shape preset is otherwise 94 clicks with half of
    /// them spent re-arming. Off is what you want while adding VARIANTS — the
    /// second, third and fourth tile for one shape — so it is a checkbox beside
    /// the grid rather than a rule of the tool. Clicking a rule that already has
    /// a tile never advances either way, because asking for a filled rule is
    /// almost always asking to add to it.
    pub(crate) fill_advance: bool,
    /// The rectangle the Select tool has, `(x0, y0, x1, y1)` inclusive.
    pub(crate) selection: Option<(i32, i32, i32, i32)>,
    /// The clipboard: a stamp lifted by Copy.
    pub(crate) clipboard: Option<Stamp>,
    /// Overlay the grid lines of the active layer.
    pub(crate) show_grid: bool,
    /// Overlay the tileset's collision shapes — the only way to see that a tile
    /// you thought was solid is not.
    pub(crate) show_collision: bool,
    /// Which tileset the tab is editing (a path into [`TileStore`]).
    pub(crate) editing: Option<String>,
    /// The palette tiles whose properties the tab is EDITING.
    ///
    /// A set rather than one cell, because "these forty tiles are all solid" and
    /// "these six are the same slope" are the two things setting up a sheet
    /// actually consists of, and doing them one tile at a time is where the
    /// afternoon goes. Every control in the TILE section writes to all of it.
    ///
    /// Driven by the palette: a click selects one, a drag selects the band,
    /// ctrl-click adds or removes one so the set does not have to be a
    /// rectangle. Kept sorted so the primary tile — the one a shape is drawn on
    /// — is stable rather than whichever the hash landed on first.
    pub(crate) inspect: BTreeSet<u32>,

    // ---- live gesture -----------------------------------------------------
    /// Where a drag or stroke began, in tile coordinates.
    pub(crate) from: Option<(i32, i32)>,
    /// The last square a stroke painted, so a stroke does not re-place the same
    /// square sixty times a second (each write would be a change to coalesce).
    pub(crate) last: Option<(i32, i32)>,
    /// Whether the gesture in flight changed anything — a press that paints
    /// nothing should not leave an empty step in the undo stack.
    pub(crate) touched: bool,
    /// A press is in flight.
    pub(crate) down: bool,
}

impl Default for TileTools {
    fn default() -> Self {
        Self {
            tool: TileTool::Brush,
            layer: None,
            stamp: Stamp::one(0),
            xform: TileXform::NONE,
            palette: Some((0, 0, 1, 1)),
            page: 0,
            group: None,
            auto_retile: true,
            fill_mask: None,
            fill_advance: true,
            selection: None,
            clipboard: None,
            show_grid: true,
            show_collision: false,
            editing: None,
            inspect: BTreeSet::new(),
            from: None,
            last: None,
            touched: false,
            down: false,
        }
    }
}

impl TileTools {
    /// The tile a single-tile control acts on: the lowest of the edit selection.
    ///
    /// Lowest rather than "the last one clicked" so that re-opening the tab, or
    /// re-selecting the same rectangle, lands on the same tile — a shape editor
    /// whose canvas moved when you were not looking is worse than one that
    /// sometimes picks a tile you did not mean.
    pub(crate) fn primary(&self) -> Option<u32> {
        self.inspect.iter().copied().next()
    }

    /// Select exactly `cell` — what an ordinary click does.
    pub(crate) fn inspect_one(&mut self, cell: u32) {
        self.inspect.clear();
        self.inspect.insert(cell);
    }

    /// The stamp as it would actually be placed: the palette selection, turned by
    /// the current orientation.
    ///
    /// Computed rather than stored so the palette preview and the placement come
    /// from the same expression — a preview that shows one orientation and places
    /// another is the bug this shape exists to make impossible.
    pub(crate) fn armed(&self) -> Stamp {
        if self.stamp.cols <= 1 && self.stamp.rows <= 1 {
            self.stamp.reoriented(self.xform)
        } else {
            // A multi-square stamp turns as a whole: layout AND each square.
            let mut s = self.stamp.clone();
            for _ in 0..(self.xform.rot & 3) {
                s = s.rotated_cw();
            }
            if self.xform.flip_x {
                s = s.flipped_x();
            }
            s
        }
    }
}

/// Add a tilemap node's colliders to a sim — the ONE implementation, called by
/// both the play sim (`play.rs`) and the hidden server's (`net.rs`).
///
/// Two copies of this would be two answers to "where is the floor", and the
/// symptom would be a client walking through ground the server thinks is solid —
/// which reads as a netcode bug and is not one. The two static-collider builders
/// are already parallel copies of each other for meshes and primitives; this is
/// the one shape that does not join them.
///
/// Depth is half a tile each way: a 2D collider has to have SOME depth to be a
/// box, and the tile's own size is the only defensible choice — it keeps a
/// character with any thickness inside the layer rather than passing through a
/// paper-thin wall.
pub(crate) fn add_tilemap_colliders(
    sim: &mut floptle_physics::Sim,
    store: &TileStore,
    xf: &floptle_core::transform::Transform,
    matter: &Matter,
    layer: floptle_physics::StaticTag,
) -> usize {
    use floptle_core::math::Vec3;
    let Matter::Tilemap { cols, rows, tile, data, tileset } = matter else { return 0 };
    if tileset.is_empty() {
        return 0; // art only — nothing here claims to be solid
    }
    let Some(set) = store.get(tileset) else { return 0 };
    let shapes = floptle_tiles::collision_shapes(*cols, *rows, *tile, data, set);
    let s = xf.scale;
    let depth = (*tile * 0.5 * s.z.abs().max(1e-3)).max(1e-3);
    for b in &shapes.boxes {
        // Each box's centre is in the node's LOCAL frame, so it goes through the
        // node's rotation and scale exactly like the mesh does: a rotated or
        // scaled tilemap collides where it draws.
        let local = Vec3::new(b.cx * s.x, b.cy * s.y, 0.0);
        sim.add_static_box(
            xf.translation + (xf.rotation * local).as_dvec3(),
            Vec3::new((b.hx * s.x.abs()).max(1e-4), (b.hy * s.y.abs()).max(1e-4), depth),
            xf.rotation,
            layer,
        );
    }
    // Hand-drawn outlines — the slopes. Each goes in as an extruded polygon, so
    // a ramp is a ramp in the sim and not the box around it. Anchored at its own
    // centroid-free bounding centre for the same reason the boxes are: the
    // points are baked relative to a point the node's transform places, so the
    // f64 residuals stay small however far out the level sits (ADR-0015).
    for p in &shapes.polys {
        let mid = p.pts.iter().fold([0.0f32, 0.0], |a, q| [a[0] + q[0], a[1] + q[1]]);
        let n = p.pts.len() as f32;
        let (mx, my) = (mid[0] / n * s.x, mid[1] / n * s.y);
        let pts: Vec<floptle_core::math::Vec2> = p
            .pts
            .iter()
            .map(|q| floptle_core::math::Vec2::new(q[0] * s.x - mx, q[1] * s.y - my))
            .collect();
        let local = Vec3::new(mx, my, 0.0);
        sim.add_static_poly(
            xf.translation + (xf.rotation * local).as_dvec3(),
            &pts,
            depth,
            xf.rotation,
            layer,
        );
    }
    shapes.len()
}

/// Where a project keeps its tilesets.
pub(crate) fn tileset_dir(root: &Path) -> PathBuf {
    root.join(floptle_tiles::TILESET_DIR)
}

impl Editor {
    /// Load every `<project>/tilesets/*.tileset.ron`.
    ///
    /// Called on project open and on scene switch, because a scene's tilemaps may
    /// reference a tileset the last scene never touched.
    pub(crate) fn adopt_tilesets(&mut self) {
        let dir = tileset_dir(&self.project_root);
        let Ok(entries) = floptle_vfs::read_dir(&dir) else {
            // No folder yet is the normal state of a project that has not made a
            // tileset. Not an error, and not something to report.
            return;
        };
        for entry in entries {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            if !name.ends_with(floptle_tiles::TILESET_EXT) {
                continue;
            }
            let rel = format!("{}/{name}", floptle_tiles::TILESET_DIR);
            if self.tiles.dirty.contains(&rel) {
                continue; // unsaved edits win over what is on disk
            }
            match floptle_vfs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| {
                TileSet::from_ron(&t).map_err(|e| e.to_string())
            }) {
                Ok(set) => {
                    self.tiles.load_failed.remove(&rel);
                    self.tiles.sets.insert(rel, set);
                }
                Err(err) => {
                    // Remember the failure so nothing writes a blank tileset over
                    // the file. Reported once per load rather than per frame.
                    if self.tiles.load_failed.insert(rel.clone()) {
                        self.console.push(
                            floptle_script::LogLevel::Error,
                            format!(
                                "tileset '{rel}' could not be read ({err}) — it will NOT be \
                                 overwritten. Fix or remove the file; tiles from it collide \
                                 with nothing until then."
                            ),
                            None,
                        );
                    }
                }
            }
        }
    }

    /// Write out every tileset with unsaved edits.
    pub(crate) fn save_tilesets(&mut self) {
        if self.tiles.dirty.is_empty() {
            return;
        }
        let dir = tileset_dir(&self.project_root);
        if let Err(e) = floptle_vfs::create_dir_all(&dir) {
            self.console.push(
                floptle_script::LogLevel::Error,
                format!("could not create {}: {e}", dir.display()),
                None,
            );
            return;
        }
        let paths: Vec<String> = self.tiles.dirty.iter().cloned().collect();
        for rel in paths {
            if self.tiles.load_failed.contains(&rel) {
                continue; // never write over a file we could not read
            }
            let Some(set) = self.tiles.sets.get_mut(&rel) else {
                self.tiles.dirty.remove(&rel);
                continue;
            };
            set.prune();
            let Some(name) = rel.rsplit('/').next() else { continue };
            let text = match set.to_ron() {
                Ok(t) => t,
                Err(e) => {
                    self.console.push(
                        floptle_script::LogLevel::Error,
                        format!("tileset '{rel}' could not be written: {e}"),
                        None,
                    );
                    continue;
                }
            };
            match floptle_vfs::write(dir.join(name), text) {
                Ok(()) => {
                    self.tiles.dirty.remove(&rel);
                }
                Err(e) => self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("could not write {rel}: {e}"),
                    None,
                ),
            }
        }
    }

    /// Create a tileset for a sheet and return its project-relative path.
    pub(crate) fn new_tileset(&mut self, name: &str, texture: &str, cols: u32, rows: u32) -> String {
        // A name that is already taken gets a suffix rather than replacing what is
        // there — overwriting somebody's tileset because two sheets share a file
        // name is not a recoverable mistake.
        let base: String = name
            .chars()
            .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
            .collect();
        let base = if base.trim().is_empty() { "tileset".to_string() } else { base };
        let mut candidate = base.clone();
        let mut n = 2;
        while self.tiles.sets.contains_key(&floptle_tiles::tileset_path(&candidate)) {
            candidate = format!("{base}-{n}");
            n += 1;
        }
        let rel = floptle_tiles::tileset_path(&candidate);
        self.tiles.sets.insert(
            rel.clone(),
            TileSet {
                name: candidate,
                texture: texture.to_string(),
                sheet_cols: cols.max(1),
                sheet_rows: rows.max(1),
                ..Default::default()
            },
        );
        self.tiles.dirty.insert(rel.clone());
        rel
    }

    /// The tileset the active tile layer references, if it has one and it loaded.
    pub(crate) fn active_tileset(&self) -> Option<&TileSet> {
        let e = self.tile_tools.layer?;
        let Some(Matter::Tilemap { tileset, .. }) = self.world.get::<Matter>(e) else {
            return None;
        };
        (!tileset.is_empty()).then(|| self.tiles.get(tileset))?
    }

    /// Every tilemap node in the scene, in hierarchy order — the tab's layer list.
    pub(crate) fn tile_layers(&self) -> Vec<Entity> {
        self.world
            .query::<Matter>()
            .filter_map(|(e, m)| matches!(m, Matter::Tilemap { .. }).then_some(e))
            .collect()
    }

    /// Which square of the active layer the cursor is over.
    ///
    /// Intersects the cursor ray with the layer's own PLANE (the node's local
    /// XY), rather than assuming z = 0 in world space: a tilemap parented into a
    /// rotated rig, or one of several parallax layers at different Z, still
    /// answers about itself. Returns `None` when the ray misses the plane or lands
    /// outside the grid.
    pub(crate) fn tile_cell_under(&self, cursor: Vec2) -> Option<(i32, i32)> {
        use floptle_core::math::Vec3;
        let e = self.tile_tools.layer?;
        let Some(Matter::Tilemap { cols, rows, tile, .. }) = self.world.get::<Matter>(e) else {
            return None;
        };
        let (cols, rows, tile) = (*cols, *rows, *tile);
        if cols == 0 || rows == 0 || tile <= 0.0 {
            return None;
        }
        let (ro, rd) = self.map_cursor_ray(cursor)?;
        let xf = floptle_core::world_transform(&self.world, e);
        let normal = xf.rotation * Vec3::Z;
        let denom = rd.dot(normal);
        if denom.abs() < 1e-6 {
            return None; // edge-on: there is no square under the pointer
        }
        let t = (xf.translation - ro).as_vec3().dot(normal) / denom;
        if t <= 0.0 {
            return None; // the plane is behind the camera
        }
        let hit = ro + (rd * t).as_dvec3();
        // World → the node's local frame. Scale divides out per axis; a collapsed
        // axis has no squares to point at.
        let rel = xf.rotation.inverse() * (hit - xf.translation).as_vec3();
        if xf.scale.x.abs() < 1e-9 || xf.scale.y.abs() < 1e-9 {
            return None;
        }
        let (lx, ly) = (rel.x / xf.scale.x, rel.y / xf.scale.y);
        let (w, h) = (cols as f32 * tile * 0.5, rows as f32 * tile * 0.5);
        let fx = (lx + w) / tile;
        // Row 0 is the TOP, so the row index counts down from +h.
        let fy = (h - ly) / tile;
        let (x, y) = (fx.floor() as i32, fy.floor() as i32);
        (x >= 0 && y >= 0 && x < cols as i32 && y < rows as i32).then_some((x, y))
    }

    /// Run `f` over the active layer's grid, marking the scene dirty if anything
    /// changed.
    ///
    /// Every edit goes through here, which is what keeps "did this change
    /// anything" and "is the scene dirty" from drifting apart — an edit that
    /// reports a change without setting the flag loses work on close, and one
    /// that sets it without changing anything makes a save prompt appear for
    /// nothing.
    fn with_grid<R>(&mut self, f: impl FnOnce(&mut TileGrid<'_>) -> R) -> Option<R> {
        let e = self.tile_tools.layer?;
        let Some(Matter::Tilemap { cols, rows, data, .. }) = self.world.get_mut::<Matter>(e) else {
            return None;
        };
        let (cols, rows) = (*cols, *rows);
        let mut grid = TileGrid::new(cols, rows, data);
        Some(f(&mut grid))
    }

    /// The autotiler for the active layer, if its tileset has groups.
    fn active_autotiler(&self) -> Option<(TileSet, Autotiler)> {
        let set = self.active_tileset()?.clone();
        let at = Autotiler::build(&set);
        Some((set, at))
    }

    /// Retile a region of the active layer, if a group is armed and auto-retile
    /// is on.
    fn retile_region(&mut self, a: (i32, i32), b: (i32, i32)) {
        if !self.tile_tools.auto_retile {
            return;
        }
        let Some((set, at)) = self.active_autotiler() else { return };
        self.with_grid(|g| g.retile(a, b, &set, &at));
    }

    /// What a brush stroke places at `(x, y)`: the armed stamp, or — when a group
    /// is armed — that group's tile for the neighbourhood.
    ///
    /// A group's tile is resolved AFTER the write by the retile pass, so this only
    /// has to place *a* tile of the group for the square to join it. It places the
    /// group's lowest-numbered tile, and the retile then corrects it along with
    /// every neighbour. Trying to resolve it here would read a neighbourhood the
    /// same stroke is still changing.
    fn stroke_stamp(&self) -> Stamp {
        if let Some(group) = self.tile_tools.group
            && let Some(set) = self.active_tileset()
            && let Some(&cell) = set.group_cells(group).first()
        {
            return Stamp::one(cell);
        }
        self.tile_tools.armed()
    }

    /// A press in the Scene view with the Tiles tool held. Returns whether it was
    /// consumed (so the caller does not also pick or box-select).
    pub(crate) fn tile_press(&mut self, cursor: Vec2) -> bool {
        if self.tile_tools.layer.is_none() {
            self.tile_note("pick a tile layer in the ▦ Tiles tab first");
            return false;
        }
        let Some((x, y)) = self.tile_cell_under(cursor) else {
            // Off the grid: not consumed, so a click into empty space can still
            // select a different node. Swallowing it would make the tool a trap.
            return false;
        };
        self.tile_tools.down = true;
        self.tile_tools.touched = false;
        self.tile_tools.from = Some((x, y));
        self.tile_tools.last = None;

        match self.tile_tools.tool {
            TileTool::Pick => {
                // The eyedropper is immediate and NOT an undoable edit: it changes
                // the tool, not the map.
                if let Some(p) = self.with_grid(|g| g.get(x, y)).flatten()
                    && p != floptle_core::EMPTY_TILE
                {
                    self.tile_tools.stamp = Stamp::one(floptle_core::tile_index(p));
                    self.tile_tools.xform = floptle_core::tile_xform(p);
                    self.tile_tools.palette = None;
                    self.tile_tools.inspect_one(floptle_core::tile_index(p));
                }
                self.tile_tools.down = false;
                true
            }
            TileTool::Bucket => {
                self.begin_edit();
                let stamp = self.stroke_stamp();
                let fill = stamp.data.first().copied().unwrap_or(floptle_core::EMPTY_TILE);
                let changed = self.with_grid(|g| g.flood_fill(x, y, fill)).unwrap_or(false);
                if changed {
                    self.tile_tools.touched = true;
                    // A bucket can touch the whole grid, so the retile has to as
                    // well — a region around the click would leave the rest of the
                    // filled area edged against its old neighbours.
                    let (cols, rows) = self.tile_layer_size().unwrap_or((0, 0));
                    self.retile_region((0, 0), (cols as i32 - 1, rows as i32 - 1));
                    self.scene_dirty = true;
                }
                self.tile_tools.down = false;
                true
            }
            tool if tool.is_stroke() => {
                self.begin_edit();
                self.tile_paint_at(x, y);
                true
            }
            _ => true, // a rubber band: nothing happens until the drag or release
        }
    }

    /// Paint one square (or stamp) of a stroke.
    fn tile_paint_at(&mut self, x: i32, y: i32) {
        if self.tile_tools.last == Some((x, y)) {
            return; // same square as last frame — nothing new to do
        }
        self.tile_tools.last = Some((x, y));
        let erase = self.tile_tools.tool == TileTool::Erase;
        let stamp = if erase { Stamp::one(floptle_core::EMPTY_TILE) } else { self.stroke_stamp() };
        let (sc, sr) = (stamp.cols.max(1) as i32, stamp.rows.max(1) as i32);
        let changed = self.with_grid(|g| g.stamp(x, y, &stamp, false)).unwrap_or(false);
        if changed {
            self.tile_tools.touched = true;
            self.scene_dirty = true;
            self.retile_region((x, y), (x + sc - 1, y + sr - 1));
        }
    }

    /// Per-frame update while a tile gesture is in flight.
    pub(crate) fn tile_frame_update(&mut self, cursor: Option<Vec2>) {
        if !self.tile_tools.down || !self.tile_tools.tool.is_stroke() {
            return;
        }
        let Some(cursor) = cursor else { return };
        if let Some((x, y)) = self.tile_cell_under(cursor) {
            self.tile_paint_at(x, y);
        }
    }

    /// A release in the Scene view: commit a rubber-band tool.
    pub(crate) fn tile_release(&mut self, cursor: Option<Vec2>) {
        if !self.tile_tools.down {
            return;
        }
        self.tile_tools.down = false;
        let from = self.tile_tools.from.take();
        self.tile_tools.last = None;
        if !self.tile_tools.tool.is_drag() {
            return;
        }
        let (Some(a), Some(cursor)) = (from, cursor) else { return };
        // A drag that ends off the grid commits to the last square that WAS on it,
        // which is what "drag past the edge to fill to the edge" means.
        let b = self.tile_cell_under(cursor).unwrap_or(a);

        match self.tile_tools.tool {
            TileTool::Select => {
                self.tile_tools.selection =
                    Some((a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1)));
                return; // selecting is not an edit
            }
            TileTool::Move => {
                let Some((x0, y0, x1, y1)) = self.tile_tools.selection else {
                    self.tile_note("drag out a selection with ▢ Select first");
                    return;
                };
                let (dx, dy) = (b.0 - a.0, b.1 - a.1);
                if dx == 0 && dy == 0 {
                    return;
                }
                self.begin_edit();
                let changed =
                    self.with_grid(|g| g.move_rect((x0, y0), (x1, y1), dx, dy)).unwrap_or(false);
                if changed {
                    self.tile_tools.selection = Some((x0 + dx, y0 + dy, x1 + dx, y1 + dy));
                    self.scene_dirty = true;
                    // Both ends: the hole left behind and the ground moved onto.
                    self.retile_region((x0, y0), (x1, y1));
                    self.retile_region((x0 + dx, y0 + dy), (x1 + dx, y1 + dy));
                }
                return;
            }
            _ => {}
        }

        self.begin_edit();
        let stamp = self.stroke_stamp();
        let cell = stamp.data.first().copied().unwrap_or(floptle_core::EMPTY_TILE);
        // Read the tool out BEFORE the closure: `with_grid` takes `&mut self` and
        // the closure would otherwise hold a borrow of `self.tile_tools` across it.
        let tool = self.tile_tools.tool;
        let changed = self
            .with_grid(|g| match tool {
                TileTool::Rect => g.fill_rect(a, b, cell),
                TileTool::Frame => g.stroke_rect(a, b, cell),
                TileTool::Line => g.line(a, b, cell),
                _ => false,
            })
            .unwrap_or(false);
        if changed {
            self.scene_dirty = true;
            self.retile_region(a, b);
        }
    }

    /// The active layer's `(cols, rows)`.
    pub(crate) fn tile_layer_size(&self) -> Option<(u32, u32)> {
        let e = self.tile_tools.layer?;
        match self.world.get::<Matter>(e) {
            Some(Matter::Tilemap { cols, rows, .. }) => Some((*cols, *rows)),
            _ => None,
        }
    }

    // ---- selection operations (the tab's buttons and the shortcuts) ---------

    /// Copy the selection to the tile clipboard.
    pub(crate) fn tile_copy(&mut self) {
        let Some((x0, y0, x1, y1)) = self.tile_tools.selection else { return };
        let lifted = self.with_grid(|g| g.copy_rect((x0, y0), (x1, y1)));
        if let Some(s) = lifted {
            let n = s.data.len();
            self.tile_tools.clipboard = Some(s);
            self.tile_note(&format!("copied {n} squares"));
        }
    }

    /// Make the clipboard the brush, so a paste is a placement you can see first.
    ///
    /// Pasting straight into the map at a remembered position is the version that
    /// needs an undo to correct; pasting onto the brush means the next click puts
    /// it exactly where you aimed.
    pub(crate) fn tile_paste(&mut self) {
        let Some(s) = self.tile_tools.clipboard.clone() else {
            self.tile_note("nothing copied yet");
            return;
        };
        self.tile_tools.stamp = s;
        self.tile_tools.xform = TileXform::NONE;
        self.tile_tools.palette = None;
        self.tile_tools.tool = TileTool::Brush;
        self.tile_note("the clipboard is now the brush — click to place it");
    }

    pub(crate) fn tile_clear_selection(&mut self) {
        let Some((x0, y0, x1, y1)) = self.tile_tools.selection else { return };
        self.begin_edit();
        let changed = self
            .with_grid(|g| g.fill_rect((x0, y0), (x1, y1), floptle_core::EMPTY_TILE))
            .unwrap_or(false);
        if changed {
            self.scene_dirty = true;
            self.retile_region((x0, y0), (x1, y1));
        }
    }

    /// Re-orient the selection (or, with none, the brush).
    pub(crate) fn tile_reorient_selection(&mut self, turn: bool, flip_x: bool, flip_y: bool) {
        let apply = |xf: TileXform| {
            let mut xf = xf;
            if turn {
                xf = xf.rotated_cw();
            }
            if flip_x {
                xf = xf.flipped_x();
            }
            if flip_y {
                xf = xf.flipped_y();
            }
            xf
        };
        let Some((x0, y0, x1, y1)) = self.tile_tools.selection else {
            self.tile_tools.xform = apply(self.tile_tools.xform);
            return;
        };
        self.begin_edit();
        // Each square keeps its own cell and turns from its own orientation, so a
        // selection of tiles already at different angles all turn by one step
        // rather than being flattened to one angle.
        let changed = self
            .with_grid(|g| {
                let mut hit = false;
                for y in y0..=y1 {
                    for x in x0..=x1 {
                        if let Some(p) = g.get(x, y)
                            && p != floptle_core::EMPTY_TILE
                        {
                            let next =
                                floptle_core::tile_reoriented(p, apply(floptle_core::tile_xform(p)));
                            hit |= g.set(x, y, next);
                        }
                    }
                }
                hit
            })
            .unwrap_or(false);
        if changed {
            self.scene_dirty = true;
        }
    }

    /// Resize the active layer.
    pub(crate) fn tile_resize(&mut self, cols: u32, rows: u32, ox: i32, oy: i32) {
        let Some(e) = self.tile_tools.layer else { return };
        self.begin_edit();
        let Some(Matter::Tilemap { cols: c, rows: r, data, .. }) = self.world.get_mut::<Matter>(e)
        else {
            return;
        };
        let (nc, nr, next) =
            TileGrid::new(*c, *r, data).resized(cols.max(1), rows.max(1), ox, oy);
        *c = nc;
        *r = nr;
        *data = next;
        self.tile_tools.selection = None;
        self.scene_dirty = true;
    }

    /// Retile the whole active layer — the "my autotile rules changed, fix the
    /// level" button.
    pub(crate) fn tile_retile_all(&mut self) {
        let Some((cols, rows)) = self.tile_layer_size() else { return };
        let Some((set, at)) = self.active_autotiler() else {
            self.tile_note("this layer's tileset has no autotile groups");
            return;
        };
        self.begin_edit();
        let n = self
            .with_grid(|g| g.retile((0, 0), (cols as i32 - 1, rows as i32 - 1), &set, &at))
            .unwrap_or(0);
        self.scene_dirty = n > 0;
        self.tile_note(&format!("retiled {n} squares"));
    }

    /// A one-line note in the Console, for the things a tool has to say.
    pub(crate) fn tile_note(&mut self, msg: &str) {
        self.console.push(floptle_script::LogLevel::Debug, format!("tiles: {msg}"), None);
    }

    /// Every tileset the CURRENT SCENE references, for lending to the script host
    /// and for building collision.
    ///
    /// Keyed by the path the nodes name, so a node whose tileset failed to load
    /// simply has no entry — `tm:solid` then answers `false` and the load failure
    /// was already reported once, rather than every frame.
    pub(crate) fn scene_tilesets(&self) -> HashMap<String, TileSet> {
        let mut out = HashMap::new();
        for (_, m) in self.world.query::<Matter>() {
            if let Matter::Tilemap { tileset, .. } = m
                && !tileset.is_empty()
                && !out.contains_key(tileset)
                && let Some(set) = self.tiles.get(tileset)
            {
                out.insert(tileset.clone(), set.clone());
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_its_own_shortcut_and_glyph() {
        // The toolbar and the key handler both read `ALL`, so a duplicate key
        // would make one tool unreachable — silently, and only for whoever
        // reaches for it.
        let mut keys = HashSet::new();
        let mut glyphs = HashSet::new();
        for t in TileTool::ALL {
            assert!(keys.insert(t.key()), "{:?} shares a key with another tool", t);
            assert!(glyphs.insert(t.glyph()), "{:?} shares a glyph", t);
            assert!(!t.label().is_empty());
            assert!(!t.hint().is_empty(), "{:?} has no hint", t);
        }
        assert_eq!(keys.len(), TileTool::ALL.len());
    }

    #[test]
    fn a_stroke_tool_is_not_a_drag_tool() {
        // The press handler branches on these, and a tool that claimed both would
        // paint on press AND commit a rectangle on release.
        for t in TileTool::ALL {
            assert!(!(t.is_stroke() && t.is_drag()), "{t:?} is both a stroke and a drag");
        }
        assert!(TileTool::Brush.is_stroke());
        assert!(TileTool::Rect.is_drag());
        assert!(!TileTool::Pick.is_stroke() && !TileTool::Pick.is_drag(), "pick is immediate");
        assert!(!TileTool::Bucket.is_stroke() && !TileTool::Bucket.is_drag());
    }

    /// The armed stamp must be what the preview shows: the same function, from the
    /// same state. A single square re-orients; a rectangle turns as a whole.
    #[test]
    fn the_armed_stamp_turns_with_the_orientation_buttons() {
        let mut t = TileTools { stamp: Stamp::one(5), ..Default::default() };
        assert_eq!(t.armed().data, vec![5]);

        t.xform = TileXform::new(1, false);
        let armed = t.armed();
        assert_eq!(floptle_core::tile_index(armed.data[0]), 5, "the cell is unchanged");
        assert_eq!(floptle_core::tile_xform(armed.data[0]), TileXform::new(1, false));

        // A 3x1 run turned a quarter-turn is 1x3.
        t.stamp = Stamp { cols: 3, rows: 1, data: vec![1, 2, 3] };
        let armed = t.armed();
        assert_eq!((armed.cols, armed.rows), (1, 3));
        // …and mirroring a turned multi-square stamp is still one of the eight.
        t.xform = TileXform::new(1, true);
        let armed = t.armed();
        assert_eq!((armed.cols, armed.rows), (1, 3));
    }

    #[test]
    fn a_fresh_tool_state_is_a_usable_one() {
        // The default has to paint something: a brush armed with nothing is a tool
        // that appears broken on first click.
        let t = TileTools::default();
        assert_eq!(t.tool, TileTool::Brush);
        assert!(!t.armed().is_empty(), "the default brush must place a tile");
        assert!(t.auto_retile, "autotiling you have to remember to switch on looks broken");
        assert!(t.show_grid);
    }

    #[test]
    fn the_store_marks_a_set_dirty_only_when_it_is_handed_out_mutably() {
        let mut store = TileStore::default();
        store.sets.insert("tilesets/a.tileset.ron".into(), TileSet::default());
        assert!(store.get("tilesets/a.tileset.ron").is_some());
        assert!(store.dirty.is_empty(), "reading is not an edit");
        let _ = store.get_mut("tilesets/a.tileset.ron");
        assert_eq!(store.dirty.len(), 1);
        assert_eq!(store.paths(), vec!["tilesets/a.tileset.ron".to_string()]);
    }
}

/// The ◫ Tiles overlay for one frame: everything the Scene view draws on top of
/// the active tile layer, already projected to screen.
///
/// Built once a frame and consumed by `scene_tab`, the same shape `MapViz` uses —
/// the projection needs the camera and the render size, which the tab does not
/// have, and doing it per-segment in the paint call would re-derive the
/// view-projection for every line.
#[derive(Default)]
pub(crate) struct TileViz {
    /// The layer's grid lines.
    pub(crate) grid: Vec<(Vec2, Vec2)>,
    /// The tileset's collision boxes, as closed rings.
    pub(crate) collision: Vec<Vec<Vec2>>,
    /// The square (or stamp) under the cursor, as a closed ring.
    pub(crate) cursor: Option<Vec<Vec2>>,
    /// The rubber band of a drag in flight, as a closed ring.
    pub(crate) band: Option<Vec<Vec2>>,
    /// The current selection, as a closed ring.
    pub(crate) selection: Option<Vec<Vec2>>,
    /// The layer's outer edge — where the map ENDS, which is otherwise invisible
    /// on an empty grid and is the first thing you need to see.
    pub(crate) bounds: Vec<Vec2>,
}

impl Editor {
    /// Rebuild the ◫ Tiles overlay. Cheap and skipped entirely unless the tile
    /// tool is held.
    pub(crate) fn tile_frame_viz(&mut self) {
        use floptle_core::math::Vec3;
        self.tile_viz = None;
        if self.tool != crate::gizmo::Tool::Tiles || self.playing {
            return;
        }
        let Some(e) = self.tile_tools.layer else { return };
        let Some(Matter::Tilemap { cols, rows, tile, .. }) = self.world.get::<Matter>(e) else {
            return;
        };
        let (cols, rows, tile) = (*cols, *rows, *tile);
        if cols == 0 || rows == 0 || tile <= 0.0 {
            return;
        }
        let Some(gpu) = self.gpu.as_ref() else { return };
        let (w, h) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
        let cam = self.camera.render_camera();
        let vp = cam.view_proj(w / h);
        let xf = floptle_core::world_transform(&self.world, e);
        let project = |p: Vec3| {
            let wp = xf.translation + (xf.rotation * (xf.scale * p)).as_dvec3();
            crate::viz::project(wp, cam.world_position, vp, w, h)
        };
        // Grid space → the layer's local frame. Row 0 is the TOP, matching the
        // mesh and every coordinate the tools use.
        let (hw, hh) = (cols as f32 * tile * 0.5, rows as f32 * tile * 0.5);
        let corner = |x: f32, y: f32| Vec3::new(x * tile - hw, hh - y * tile, 0.0);
        let ring = |x0: f32, y0: f32, x1: f32, y1: f32| -> Option<Vec<Vec2>> {
            let pts: Vec<Vec2> = [(x0, y0), (x1, y0), (x1, y1), (x0, y1)]
                .into_iter()
                .filter_map(|(x, y)| project(corner(x, y)))
                .collect();
            (pts.len() == 4).then_some(pts)
        };

        let mut viz = TileViz {
            bounds: ring(0.0, 0.0, cols as f32, rows as f32).unwrap_or_default(),
            ..Default::default()
        };

        if self.tile_tools.show_grid {
            // A grid of 4,096 squares is 130 projected line segments; a grid of a
            // million would be 2,000, and drawing lines closer together than a
            // pixel is worse than not drawing them. So: skip whole lines once they
            // would crowd, chosen from the on-screen spacing rather than a fixed
            // count, so zooming in brings them back.
            let step = {
                let (a, b) = (project(corner(0.0, 0.0)), project(corner(1.0, 0.0)));
                match (a, b) {
                    (Some(a), Some(b)) => {
                        let px = (b - a).length().max(0.01);
                        // At least 6 screen pixels between lines.
                        ((6.0 / px).ceil() as u32).max(1)
                    }
                    _ => 1,
                }
            };
            for x in (0..=cols).step_by(step as usize) {
                if let (Some(a), Some(b)) =
                    (project(corner(x as f32, 0.0)), project(corner(x as f32, rows as f32)))
                {
                    viz.grid.push((a, b));
                }
            }
            for y in (0..=rows).step_by(step as usize) {
                if let (Some(a), Some(b)) =
                    (project(corner(0.0, y as f32)), project(corner(cols as f32, y as f32)))
                {
                    viz.grid.push((a, b));
                }
            }
        }

        if self.tile_tools.show_collision
            && let Some(Matter::Tilemap { data, tileset, .. }) = self.world.get::<Matter>(e)
            && !tileset.is_empty()
            && let Some(set) = self.tiles.get(tileset)
        {
            // The merged boxes — the SAME ones the sim gets, so what you see is
            // what a character walks on. Drawing per-tile outlines instead would
            // show a grid that does not exist in the physics world.
            for b in floptle_tiles::collision_shapes(cols, rows, tile, data, set).boxes {
                let pts: Vec<Vec2> = [
                    (b.cx - b.hx, b.cy - b.hy),
                    (b.cx + b.hx, b.cy - b.hy),
                    (b.cx + b.hx, b.cy + b.hy),
                    (b.cx - b.hx, b.cy + b.hy),
                ]
                .into_iter()
                .filter_map(|(x, y)| project(Vec3::new(x, y, 0.0)))
                .collect();
                if pts.len() == 4 {
                    viz.collision.push(pts);
                }
            }
        }

        if let Some((x0, y0, x1, y1)) = self.tile_tools.selection {
            viz.selection = ring(x0 as f32, y0 as f32, (x1 + 1) as f32, (y1 + 1) as f32);
        }

        if let Some(cur) = self.cursor
            && let Some((cx, cy)) = self.tile_cell_under(cur)
        {
            // Mid-drag the telegraph is the RECTANGLE, not the square: what a
            // release will do, not where the pointer happens to be.
            if let Some((ax, ay)) = self.tile_tools.from.filter(|_| self.tile_tools.down) {
                let (lo_x, hi_x) = (ax.min(cx), ax.max(cx));
                let (lo_y, hi_y) = (ay.min(cy), ay.max(cy));
                viz.band =
                    ring(lo_x as f32, lo_y as f32, (hi_x + 1) as f32, (hi_y + 1) as f32);
            } else {
                let s = self.tile_tools.armed();
                viz.cursor = ring(
                    cx as f32,
                    cy as f32,
                    (cx + s.cols.max(1) as i32) as f32,
                    (cy + s.rows.max(1) as i32) as f32,
                );
            }
        }
        self.tile_viz = Some(viz);
    }
}
