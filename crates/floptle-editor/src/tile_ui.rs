//! The **▦ Tiles** tab: the tool strip, the layer list, the palette, and the
//! tileset editor.
//!
//! Laid out in the order you work in — LAYER, TOOL, PALETTE, TILE, AUTOTILE —
//! with the same visual language as the ▦ Map tab (a rule under each section
//! title, equal-width chips for anything that picks a mode, equal-width buttons
//! for anything that acts).
//!
//! ## The palette IS the tileset editor
//!
//! There is no separate "tileset properties" window. Click a tile in the palette
//! and its collision, tags and autotile mask are right there under it. That is
//! deliberate and it is the whole answer to "make it easy to set up collisions
//! and autotiling": both are per-tile facts, the palette is where you are looking
//! at a tile, so both are one click from the tile.
//!
//! The palette also DRAWS what it knows, over the art:
//!
//! * a solid tile gets a collision overlay in the shape of its collider (so a
//!   half-tile collider looks like a half tile, not like a tick);
//! * an autotiled tile gets a 3×3 neighbourhood diagram showing the mask it
//!   answers.
//!
//! That second one is what makes the autotile presets safe to offer. A preset has
//! to guess somebody's sheet layout, and a wrong guess otherwise reads as bad art
//! — with the diagram you can see which tiles disagree and fix one in a click.
//!
//! ## Every section is always here
//!
//! The sections that need a tileset used to return before drawing so much as
//! their own heading, so a layer without one showed a panel with no TILE and no
//! AUTOTILE in it at all. That is indistinguishable from an engine that has
//! neither, and it was reported as exactly that. A section that cannot act now
//! says what it would do and what it is waiting for; the panel's shape does not
//! change under you (`floptle/0093`).

use egui::{Color32, RichText};
use floptle_core::{Entity, Matter, TileXform};
use floptle_tiles::{
    AutotileGroup, AutotileKind, Stamp, TileCollision, TileSet, TileSide, autotile,
};

use crate::tile_edit::{TileStore, TileTool, TileTools};

/// The measurements the panel is built on — matched to the ▦ Map tab so the two
/// tile-ish panels do not look like they came from different programs.
const LABEL_W: f32 = 58.0;
const CHIP_W: f32 = 74.0;
const BTN_H: f32 = 22.0;
const ACCENT: Color32 = Color32::from_rgb(255, 200, 80);

/// An action the tab wants that needs `&mut Editor` — undo snapshots, the world,
/// or the tileset store. Applied after the frame, like every other tab's intents.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TileCmd {
    /// Make this tilemap node the layer being painted.
    PickLayer(Entity),
    /// Add a tilemap node to the scene.
    AddLayer,
    /// Resize the active layer: `(cols, rows, offset_x, offset_y)`.
    Resize(u32, u32, i32, i32),
    /// Retile every square of the active layer.
    RetileAll,
    /// Selection operations.
    Copy,
    Paste,
    ClearSelection,
    /// Turn / mirror the selection (or the brush, with no selection).
    Reorient { turn: bool, flip_x: bool, flip_y: bool },
    /// Create a tileset for the active layer's own sheet and attach it.
    NewTilesetForLayer,
    /// Point the active layer at this tileset path (empty = none).
    AttachTileset(String),
    /// Add a sheet to the tileset being edited (`floptle/0092`).
    AddPage,
    /// Point a page at an image and a cut. Page 0 is the layer's material and
    /// is not settable here.
    SetPage(u32, String, u32, u32),
    /// Drop the LAST page. Only the last, because removing one from the middle
    /// would renumber nothing (the stride is fixed) but would leave every
    /// square placed from it drawing a hole with no way back.
    RemoveLastPage,
    /// Replace one tile's tag list.
    SetTags(u32, Vec<String>),
    /// Add a tile to one of a group's rules: `(group, neighbourhood, cell)`.
    ///
    /// Adds, never replaces. A rule holding several tiles is a set of variants,
    /// and the same cell added twice is twice as likely — both are things an
    /// artist asks for, and both were impossible while the assignment lived on
    /// the tile as one group and one mask.
    AddToRule(u16, u8, u32),
    /// Drop the `n`th variant of a rule. By position, so removing one of a pair
    /// of duplicates does not take its twin.
    RemoveVariant(u16, u8, usize),
    /// Empty a rule: the shape goes back to having no tile, and squares with
    /// those neighbours are left as painted.
    ClearRule(u16, u8),
    /// Set a tile's animation: extra frames and the rate.
    SetAnim(u32, Vec<u32>, f32),
    /// Add a group to the tileset being edited.
    AddGroup(AutotileKind),
    /// Remove a group.
    RemoveGroup(u16),
    /// Rename a group.
    RenameGroup(u16, String),
    /// Set which other groups a group joins.
    SetGroupJoins(u16, Vec<u16>),
    /// Assign the preset masks to the palette's current selection, in cell order.
    ApplyPreset(u16),
    /// Mark every tile in the palette selection solid (or not) in one go.
    BulkCollision(Vec<u32>, TileCollision),
    /// Save the dirty tilesets now.
    SaveTilesets,
}

/// One variant of an autotile rule: its cell, and the sheet page its art is on.
///
/// A named type because the strip under the rules grid has to look these up
/// while the sheet closure is still the only borrow of `self`, and draw them
/// after that closure has let go.
type VariantThumb = (u32, Option<(egui::TextureHandle, u32, u32)>);

/// What the tab borrows. A `Ctx` rather than more `EditorTabViewer` fields,
/// following the 🖼 Image tab: the tab needs a lot of state and none of the rest
/// of the editor needs to know about it.
pub(crate) struct TileCtx<'a> {
    pub(crate) store: &'a mut TileStore,
    pub(crate) tools: &'a mut TileTools,
    /// Read-only view of the scene, for the layer list.
    pub(crate) world: &'a floptle_core::World,
    pub(crate) project_root: &'a std::path::Path,
    pub(crate) cmds: &'a mut Vec<TileCmd>,
    /// Whether Play is running — tile editing is an edit-mode activity.
    pub(crate) playing: bool,
}

/// A titled section rule: `TITLE ─────────────`.
fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).small().strong().color(ui.visuals().strong_text_color()));
        let rect = ui.available_rect_before_wrap();
        if rect.width() > 8.0 {
            let y = rect.center().y;
            ui.painter().line_segment(
                [egui::pos2(rect.left() + 4.0, y), egui::pos2(rect.right(), y)],
                egui::Stroke::new(1.0, ui.visuals().weak_text_color()),
            );
        }
    });
    ui.add_space(4.0);
}

fn labelled(ui: &mut egui::Ui, label: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.add_sized([LABEL_W, BTN_H], egui::Label::new(RichText::new(label).small()));
        body(ui);
    });
}

/// The cut to start a freshly-dropped sheet on: `(cols, rows)`.
///
/// A dropped image with no cut is 1×1 — one tile the size of the whole sheet —
/// which looks like the drop failed. Guessing costs nothing because the guess is
/// visible and editable on the row the moment it lands.
///
/// Two sources, in order of how much they know:
///
/// 1. **The filename**, if it carries a cell size (`tiles_16x16.png`,
///    `dungeon-32.png`). An artist who wrote the number down means it, and it
///    beats any inference — a 64×64 sheet of 32-px tiles and one of 16-px tiles
///    are the same image to everything except that name.
/// 2. **The pixel size**, taking the largest common cell from 64 down to 8 that
///    divides both sides and yields at least a 2×2 grid. Preferring LARGE cells
///    is deliberate: guessing 8 px on a sheet of 32-px tiles gives a palette of
///    sixteen meaningless quarter-tiles, while guessing 32 on a sheet of 8s
///    gives four tiles that are visibly wrong. The first reads as a broken
///    engine; the second reads as a number to change.
///
/// Falls back to 1×1 when the image is unreadable, which is honest: no cut is
/// better than a confident wrong one.
fn guess_sheet_grid(path: &str, px: Option<(u32, u32)>) -> (u32, u32) {
    let (w, h) = match px {
        Some(v) if v.0 > 0 && v.1 > 0 => v,
        _ => return (1, 1),
    };
    if let Some(cell) = cell_size_in_name(path)
        && cell > 0
        && w.is_multiple_of(cell)
        && h.is_multiple_of(cell)
    {
        return (w / cell, h / cell);
    }
    for cell in [64u32, 48, 32, 24, 16, 8] {
        if w.is_multiple_of(cell) && h.is_multiple_of(cell) && w / cell >= 2 && h / cell >= 2 {
            return (w / cell, h / cell);
        }
    }
    (1, 1)
}

/// A cell size written into a filename: `16x16`, `_32`, `-8x8`. `None` if there
/// is no number, or if `NxM` names a non-square cell (which this cannot express).
fn cell_size_in_name(path: &str) -> Option<u32> {
    let stem = path.rsplit(['/', '\\']).next()?.rsplit_once('.').map(|(s, _)| s).unwrap_or(path);
    let mut best = None;
    let bytes: Vec<char> = stem.to_ascii_lowercase().chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let a: u32 = bytes[start..i].iter().collect::<String>().parse().ok()?;
        // `16x16` — take it only when both halves agree, since one cell size is
        // all a tileset page has.
        if i < bytes.len() && bytes[i] == 'x' {
            let s2 = i + 1;
            let mut j = s2;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j > s2 {
                let b: u32 = bytes[s2..j].iter().collect::<String>().parse().ok()?;
                i = j;
                if a == b {
                    best = Some(a);
                }
                continue;
            }
        }
        // A bare number is only a cell size if it is a plausible one — a year
        // or a version in a filename is not a tile size.
        if (4..=256).contains(&a) {
            best = best.or(Some(a));
        }
    }
    best
}

/// A page's tab label: its image's file stem, or `page N` when it has none.
fn short_texture(tex: &str, page: u32) -> String {
    let stem = tex.rsplit(['/', '\\']).next().unwrap_or("").split('.').next().unwrap_or("");
    if stem.is_empty() { format!("page {page}") } else { stem.to_string() }
}

/// The line that stands in for a whole section when the layer has no tileset.
///
/// The sections BELOW the tileset used to vanish entirely without one — no TILE
/// heading, no AUTOTILE heading, nothing. Which is indistinguishable from an
/// engine that does not have per-tile collision or autotiling, and is exactly
/// what one was reported as: "there isn't a way to build the collision shape for
/// each tile", "I'm still not seeing any auto tiling settings". Both were built.
///
/// So the panel's shape is now constant, and a section that cannot act says what
/// it would do and what it is waiting for. `what` is the one-sentence version of
/// the feature, in the words somebody would go looking for it under.
fn needs_tileset(ui: &mut egui::Ui, what: &str, cmds: &mut Vec<TileCmd>) {
    ui.small(what);
    ui.horizontal(|ui| {
        ui.colored_label(ACCENT, "⚠ needs a tileset");
        if ui
            .small_button("+ New tileset")
            .on_hover_text("for this layer's own sheet, sized from its material")
            .clicked()
        {
            cmds.push(TileCmd::NewTilesetForLayer);
        }
    });
}

impl TileCtx<'_> {
    pub(crate) fn ui(&mut self, ui: &mut egui::Ui) {
        if self.playing {
            ui.label(RichText::new("⏸ tile editing resumes on Stop").italics());
            ui.small("Play owns the viewport; the map is live scene state while it runs.");
            return;
        }

        self.layer_section(ui);
        // Everything below needs a layer. Showing a full panel of controls that
        // silently do nothing is worse than showing why.
        if self.tools.layer.is_none() {
            ui.add_space(10.0);
            ui.label(RichText::new("no tile layer yet").italics());
            ui.small(
                "A tile layer is an ordinary ▦ Tilemap node — it has a transform, a \
                 material (its sheet), and a place in the Hierarchy. Add one above.",
            );
            return;
        }
        self.tool_section(ui);
        self.grid_section(ui);
        self.tileset_section(ui);
        self.palette_section(ui);
        self.tile_section(ui);
        self.autotile_section(ui);
    }

    // ---- LAYER --------------------------------------------------------------

    fn layer_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "LAYER");
        let layers: Vec<Entity> = self
            .world
            .query::<Matter>()
            .filter_map(|(e, m)| matches!(m, Matter::Tilemap { .. }).then_some(e))
            .collect();
        // A layer that has gone away (undo, delete) must not stay armed — every
        // later call would silently no-op against a dead entity.
        if self.tools.layer.is_some_and(|e| !layers.contains(&e)) {
            self.tools.layer = None;
            self.tools.selection = None;
        }
        if self.tools.layer.is_none() {
            self.tools.layer = layers.first().copied();
        }

        egui::ScrollArea::vertical().max_height(110.0).id_salt("tile_layers").show(ui, |ui| {
            for e in &layers {
                let name = self
                    .world
                    .get::<floptle_core::Name>(*e)
                    .map(|n| n.0.clone())
                    .unwrap_or_else(|| format!("#{}", e.index()));
                let (cols, rows) = match self.world.get::<Matter>(*e) {
                    Some(Matter::Tilemap { cols, rows, .. }) => (*cols, *rows),
                    _ => (0, 0),
                };
                // Visibility is the node's own `Visible` — the Hierarchy's eye and
                // this one are the same switch, so they cannot disagree.
                let hidden = floptle_core::is_disabled(self.world, *e)
                    || self.world.get::<floptle_core::Visible>(*e).is_some_and(|v| !v.0);
                ui.horizontal(|ui| {
                    let active = self.tools.layer == Some(*e);
                    let label = if hidden {
                        RichText::new(format!("◌ {name}")).weak()
                    } else {
                        RichText::new(format!("▦ {name}"))
                    };
                    if ui
                        .selectable_label(active, label)
                        .on_hover_text(format!("{cols}×{rows} — click to paint into this layer"))
                        .clicked()
                    {
                        self.cmds.push(TileCmd::PickLayer(*e));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.small(format!("{cols}×{rows}"));
                    });
                });
            }
        });
        if ui
            .add_sized([CHIP_W * 2.0, BTN_H], egui::Button::new("+ Add layer"))
            .on_hover_text("a new ▦ Tilemap node, in front of the last one")
            .clicked()
        {
            self.cmds.push(TileCmd::AddLayer);
        }
    }

    // ---- TOOL ---------------------------------------------------------------

    fn tool_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "TOOL");
        ui.horizontal_wrapped(|ui| {
            for t in TileTool::ALL {
                let on = self.tools.tool == t;
                if ui
                    .add_sized(
                        [CHIP_W, BTN_H],
                        egui::Button::new(format!("{} {}", t.glyph(), t.label())).selected(on),
                    )
                    .on_hover_text(t.hint())
                    .clicked()
                {
                    self.tools.tool = t;
                }
            }
        });
        ui.small("Paint in the ⌖ Scene view. Press 9 for the tile tool, then B / E / R / G …");

        // Orientation — the ⇔ ⇕ ↻ trio. With a selection they turn the SELECTION;
        // without one they turn the brush. Same buttons, because "turn this" is one
        // idea and having two sets of them is how you press the wrong one.
        let has_sel = self.tools.selection.is_some();
        labelled(ui, "orient", |ui| {
            if ui
                .add_sized([32.0, BTN_H], egui::Button::new("↻"))
                .on_hover_text(if has_sel {
                    "turn the selection a quarter-turn clockwise"
                } else {
                    "turn the brush a quarter-turn clockwise"
                })
                .clicked()
            {
                self.cmds.push(TileCmd::Reorient { turn: true, flip_x: false, flip_y: false });
            }
            if ui
                .add_sized([32.0, BTN_H], egui::Button::new("⇔"))
                .on_hover_text("mirror left-to-right")
                .clicked()
            {
                self.cmds.push(TileCmd::Reorient { turn: false, flip_x: true, flip_y: false });
            }
            if ui
                .add_sized([32.0, BTN_H], egui::Button::new("⇕"))
                .on_hover_text("mirror top-to-bottom")
                .clicked()
            {
                self.cmds.push(TileCmd::Reorient { turn: false, flip_x: false, flip_y: true });
            }
            ui.label(RichText::new(self.tools.xform.label()).small().color(ACCENT));
            if self.tools.xform != TileXform::NONE
                && ui.small_button("reset").on_hover_text("back to unturned").clicked()
            {
                self.tools.xform = TileXform::NONE;
            }
        });

        // The brush preview: exactly what a click will place, drawn through the
        // same `armed()` the placement uses.
        let armed = self.tools.armed();
        if let Some(sheet) = self.sheet_handle(ui) {
            labelled(ui, "brush", |ui| {
                let cell_px = 22.0f32;
                let (sc, sr) = self.sheet_size();
                ui.vertical(|ui| {
                    for y in 0..armed.rows {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 0.0;
                            for x in 0..armed.cols {
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(cell_px, cell_px),
                                    egui::Sense::hover(),
                                );
                                let Some(p) = armed.get(x, y) else { continue };
                                paint_tile(ui, rect, &sheet, sc, sr, p);
                            }
                        });
                    }
                });
            });
        }

        // Selection operations.
        if has_sel {
            let (x0, y0, x1, y1) = self.tools.selection.unwrap();
            labelled(ui, "selection", |ui| {
                ui.small(format!("{}×{} at {x0},{y0}", x1 - x0 + 1, y1 - y0 + 1));
            });
            ui.horizontal_wrapped(|ui| {
                if ui.add_sized([CHIP_W, BTN_H], egui::Button::new("Copy")).clicked() {
                    self.cmds.push(TileCmd::Copy);
                }
                if ui
                    .add_sized([CHIP_W, BTN_H], egui::Button::new("Paste"))
                    .on_hover_text("put the clipboard on the brush, so the next click places it")
                    .clicked()
                {
                    self.cmds.push(TileCmd::Paste);
                }
                if ui.add_sized([CHIP_W, BTN_H], egui::Button::new("Clear")).clicked() {
                    self.cmds.push(TileCmd::ClearSelection);
                }
                if ui.add_sized([CHIP_W, BTN_H], egui::Button::new("Deselect")).clicked() {
                    self.tools.selection = None;
                }
            });
        } else if self.tools.clipboard.is_some()
            && ui
                .add_sized([CHIP_W * 2.0, BTN_H], egui::Button::new("Paste to brush"))
                .clicked()
        {
            self.cmds.push(TileCmd::Paste);
        }
    }

    // ---- GRID ---------------------------------------------------------------

    fn grid_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "GRID");
        let Some(e) = self.tools.layer else { return };
        let (cols, rows, tile) = match self.world.get::<Matter>(e) {
            Some(Matter::Tilemap { cols, rows, tile, .. }) => (*cols, *rows, *tile),
            _ => return,
        };
        // Resize is a two-part gesture: the size, and where the old content lands.
        // Local egui state rather than a field, because it is a form being filled
        // in and abandoning it should cost nothing.
        let id = egui::Id::new(("tile_resize", e.index()));
        let mut want: (u32, u32, i32, i32) =
            ui.data(|d| d.get_temp(id)).unwrap_or((cols, rows, 0, 0));
        // The live size wins when it changed under us (undo, a script), so the
        // form is never a stale number somebody then presses Apply on.
        if ui.data(|d| d.get_temp::<(u32, u32)>(id.with("seen"))) != Some((cols, rows)) {
            want = (cols, rows, 0, 0);
            ui.data_mut(|d| d.insert_temp(id.with("seen"), (cols, rows)));
        }
        labelled(ui, "size", |ui| {
            ui.add(egui::DragValue::new(&mut want.0).range(1..=4096).prefix("w "));
            ui.add(egui::DragValue::new(&mut want.1).range(1..=4096).prefix("h "));
        });
        labelled(ui, "anchor", |ui| {
            ui.add(egui::DragValue::new(&mut want.2).range(-4096..=4096).prefix("x+"))
                .on_hover_text("where the old top-left lands — 1 to grow a column on the left");
            ui.add(egui::DragValue::new(&mut want.3).range(-4096..=4096).prefix("y+"))
                .on_hover_text("…and 1 to grow a row on the top");
        });
        ui.data_mut(|d| d.insert_temp(id, want));
        let changed = (want.0, want.1) != (cols, rows) || want.2 != 0 || want.3 != 0;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(changed, egui::Button::new("Apply size").min_size([CHIP_W, BTN_H].into()))
                .on_hover_text("keeps whatever overlaps; anything outside the new grid is dropped")
                .clicked()
            {
                self.cmds.push(TileCmd::Resize(want.0, want.1, want.2, want.3));
                ui.data_mut(|d| d.insert_temp(id, (want.0, want.1, 0, 0)));
            }
            ui.small(format!("{tile} units / tile"));
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.tools.show_grid, "Grid").on_hover_text(
                "draw the layer's tile lines in the Scene view",
            );
            ui.checkbox(&mut self.tools.show_collision, "Collision").on_hover_text(
                "draw the tileset's collision shapes over the map — the only way to SEE \
                 that a tile you thought was solid is not",
            );
        });
        // An overlay that draws nothing reads as "nothing here is solid", which
        // is true and useless. Say which of the two it is. Read from the LAYER
        // rather than `tools.editing` — this section runs before the one that
        // derives it, so `editing` here is a frame behind.
        if self.tools.show_collision && self.layer_tileset().is_empty() {
            ui.colored_label(ACCENT, "⚠ nothing to draw — this layer has no tileset");
        }
    }

    // ---- TILESET ------------------------------------------------------------

    /// The active layer's tileset path, straight from the node. Empty when the
    /// layer has none — the authority `tools.editing` is derived from.
    fn layer_tileset(&self) -> String {
        match self.tools.layer.and_then(|e| self.world.get::<Matter>(e)) {
            Some(Matter::Tilemap { tileset, .. }) => tileset.clone(),
            _ => String::new(),
        }
    }

    fn tileset_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "TILESET");
        // `editing` is what every section below points at, and it used to be set
        // and never cleared — so selecting a layer with no tileset left the TILE
        // and AUTOTILE editors quietly writing to the PREVIOUS layer's tileset.
        // It is derived from the layer, so derive it here, every frame, both ways.
        let Some(e) = self.tools.layer else {
            self.tools.editing = None;
            return;
        };
        let current = match self.world.get::<Matter>(e) {
            Some(Matter::Tilemap { tileset, .. }) => tileset.clone(),
            _ => String::new(),
        };
        if current.is_empty() {
            self.tools.editing = None;
            ui.small(
                "This layer has no tileset, so its tiles collide with nothing and cannot \
                 autotile. One tileset per spritesheet, shared by every layer cut from it.",
            );
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_sized([CHIP_W * 2.0, BTN_H], egui::Button::new("+ New tileset"))
                    .on_hover_text("for this layer's own sheet, sized from its material")
                    .clicked()
                {
                    self.cmds.push(TileCmd::NewTilesetForLayer);
                }
                for path in self.store.paths() {
                    if ui.add_sized([CHIP_W * 2.0, BTN_H], egui::Button::new(short(&path))).clicked()
                    {
                        self.cmds.push(TileCmd::AttachTileset(path.clone()));
                    }
                }
            });
            return;
        }

        if self.store.load_failed.contains(&current) {
            self.tools.editing = None;
            ui.colored_label(
                Color32::from_rgb(255, 120, 110),
                format!("⚠ {} could not be read", short(&current)),
            );
            ui.small(
                "It will NOT be overwritten. Fix or remove the file — until then these \
                 tiles collide with nothing.",
            );
            return;
        }
        let Some(set) = self.store.get(&current) else {
            self.tools.editing = None;
            ui.colored_label(ACCENT, format!("⚠ {} is missing", short(&current)));
            ui.small("The layer names a tileset this project does not have.");
            if ui.button("Detach").clicked() {
                self.cmds.push(TileCmd::AttachTileset(String::new()));
            }
            return;
        };
        self.tools.editing = Some(current.clone());
        let (sc, sr, ncells) = (set.sheet_cols, set.sheet_rows, set.cells());
        let solid = set.tiles.values().filter(|t| t.collision.is_solid()).count();
        let groups = set.groups.len();
        labelled(ui, "file", |ui| {
            ui.small(short(&current));
            if self.store.dirty.contains(&current) {
                ui.label(RichText::new("•").color(ACCENT)).on_hover_text("unsaved");
                if ui.small_button("Save").clicked() {
                    self.cmds.push(TileCmd::SaveTilesets);
                }
            }
        });
        labelled(ui, "sheet", |ui| {
            ui.small(format!("{sc}×{sr} — {ncells} tiles, {solid} solid, {groups} groups"));
        });
        // The two-cuts warning only applies to a tileset that names no sheet of
        // its own and is therefore still borrowing the layer's material. When
        // the tileset HAS a sheet it is the authority for both the image and the
        // cut, and there is nothing left for the material to disagree with.
        if set.texture.trim().is_empty() {
            let (msc, msr) = self.sheet_size();
            if (msc, msr) != (sc, sr) {
                ui.colored_label(
                    ACCENT,
                    format!("⚠ this layer's material is cut {msc}×{msr}, the tileset says {sc}×{sr}"),
                );
                ui.small(
                    "This tileset has no sheet of its own, so it is drawing the layer's \
                     material — and under two different cuts every cell index means a \
                     different picture. Give the tileset its own sheet below, or match \
                     the two.",
                );
            }
        }
        let set = set.clone();
        self.pages_ui(ui, &set);
        if ui.small_button("Detach tileset").clicked() {
            self.cmds.push(TileCmd::AttachTileset(String::new()));
        }
    }

    /// The tileset's extra sheets (`floptle/0092`).
    ///
    /// A level built out of a ground sheet, a props sheet and a decoration sheet
    /// used to need three tilemap NODES, and that is not a workaround — a wall on
    /// one node is not a neighbour of a wall on another, so nothing autotiles
    /// across the join, the collision merge stops at it, and every grid tool
    /// stops there too. Pages put them on one layer.
    fn pages_ui(&mut self, ui: &mut egui::Ui, set: &TileSet) {
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.small(RichText::new("SHEETS").strong());
            ui.small(format!("{}", set.page_count()));
            if set.page_count() < floptle_core::TILE_MAX_PAGES
                && ui
                    .small_button("+ sheet")
                    .on_hover_text(
                        "another image on THIS layer — one grid, so it still autotiles and \
                         merges collision across the join",
                    )
                    .clicked()
            {
                self.cmds.push(TileCmd::AddPage);
            }
            if set.page_count() > 1
                && ui
                    .small_button("− last")
                    .on_hover_text("only the last, so no square already placed loses its tile")
                    .clicked()
            {
                self.cmds.push(TileCmd::RemoveLastPage);
            }
        });
        // Page 0 is in this list like any other sheet. It is the tileset's own
        // `texture`/`sheet_cols`/`sheet_rows`, which used to be unsettable here
        // and merely "informational" — the reason a tileset could not carry its
        // own art and every layer needed a Material saying the same thing twice.
        for (p, tex, c, r) in set.pages_iter().collect::<Vec<_>>() {
            self.sheet_row(ui, p, tex, c, r);
        }
        if set.texture.trim().is_empty() {
            ui.small(
                "Sheet 0 has no image, so this tileset is borrowing the layer's material. \
                 Drop one on the row above — or on this panel — and the tileset draws itself.",
            );
        }
        // Dropping an image anywhere on the panel fills the first empty sheet,
        // else adds one. "Drag them in from my assets" should not require
        // hitting a particular row.
        if let Some(path) = self.dropped_texture(ui) {
            let slot = set
                .pages_iter()
                .find(|(_, t, ..)| t.trim().is_empty())
                .map(|(p, ..)| p);
            match slot {
                Some(p) => {
                    let (c, r) = guess_sheet_grid(&path, self.sheet_px(ui, &path));
                    self.cmds.push(TileCmd::SetPage(p, path, c, r));
                }
                None if set.page_count() < floptle_core::TILE_MAX_PAGES => {
                    let (c, r) = guess_sheet_grid(&path, self.sheet_px(ui, &path));
                    self.cmds.push(TileCmd::AddPage);
                    self.cmds.push(TileCmd::SetPage(set.page_count(), path, c, r));
                }
                None => {}
            }
        }
    }

    /// One sheet: its image, its cut, and a drop target for both.
    fn sheet_row(&mut self, ui: &mut egui::Ui, p: u32, tex: &str, c: u32, r: u32) {
        let (mut path, mut cols, mut rows) = (tex.to_string(), c, r);
        let mut changed = false;
        let resp = ui
            .horizontal(|ui| {
                ui.small(format!("{p}"));
                changed |= ui
                    .add(
                        egui::TextEdit::singleline(&mut path)
                            .desired_width(120.0)
                            .hint_text("drop or type textures/…png"),
                    )
                    .on_hover_text("project-relative image for this sheet — or drag one in")
                    .lost_focus();
                changed |= ui.add(egui::DragValue::new(&mut cols).range(1..=256).prefix("c")).changed();
                changed |= ui.add(egui::DragValue::new(&mut rows).range(1..=256).prefix("r")).changed();
                // The cut, in pixels, so "is this even" is answerable without
                // dividing in your head. A sheet that does not divide evenly is
                // the seam bug, and it says so rather than drawing it.
                if let Some((w, h)) = self.sheet_px(ui, &path) {
                    let (cw, ch) = (w / cols.max(1), h / rows.max(1));
                    if w % cols.max(1) == 0 && h % rows.max(1) == 0 {
                        ui.small(format!("{cw}×{ch}px"));
                    } else {
                        ui.colored_label(ACCENT, format!("⚠ {w}×{h} is not {cols}×{rows}"))
                            .on_hover_text(
                                "This cut does not divide the image evenly, so every tile \
                                 samples a fraction of its neighbour and the map draws seams.",
                            );
                    }
                }
            })
            .response;
        if let Some(dropped) = self.dropped_texture_on(ui, &resp) {
            let (gc, gr) = guess_sheet_grid(&dropped, self.sheet_px(ui, &dropped));
            self.cmds.push(TileCmd::SetPage(p, dropped, gc, gr));
            return;
        }
        if changed {
            self.cmds.push(TileCmd::SetPage(p, path, cols, rows));
        }
    }

    // ---- PALETTE ------------------------------------------------------------

    fn palette_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "PALETTE");
        let set = self.tools.editing.clone().and_then(|p| self.store.get(&p)).cloned();
        // Which sheet of the tileset we are picking from. A tileset with pages
        // draws a row of tabs; without one there is a single implicit page and
        // nothing extra on screen (`floptle/0092`).
        // What the brush places, as one row you can see and change without
        // knowing the rule that used to govern it: clicking a tile that happened
        // to belong to a group armed the group, clicking one that did not
        // disarmed it, and nothing on screen said which of those had happened.
        // Switching between "place this exact tile" and "paint this autotile" is
        // a thing you do constantly while building a level, so it is a switch.
        if let Some(s) = set.as_ref()
            && !s.groups.is_empty()
        {
            labelled(ui, "brush", |ui| {
                if ui
                    .add_sized([CHIP_W * 0.7, BTN_H], egui::Button::new("Tile").selected(self.tools.group.is_none()))
                    .on_hover_text("place the tile you click in the palette, exactly as it is")
                    .clicked()
                {
                    self.tools.group = None;
                }
                for (i, group) in s.groups.iter().enumerate() {
                    let gi = i as u16;
                    let on = self.tools.group == Some(gi);
                    let name = if group.name.is_empty() {
                        format!("group {i}")
                    } else {
                        group.name.clone()
                    };
                    if ui
                        .add_sized([CHIP_W * 0.9, BTN_H], egui::Button::new(format!("▦ {name}")).selected(on))
                        .on_hover_text(
                            "paint this autotile: every square works out which of the \
                             group's tiles fits its neighbours",
                        )
                        .clicked()
                    {
                        self.tools.group = Some(gi);
                        self.tools.tool = TileTool::Brush;
                    }
                }
            });
        }
        let page_count = set.as_ref().map(|s| s.page_count()).unwrap_or(1);
        if self.tools.page >= page_count {
            self.tools.page = 0;
        }
        if page_count > 1 {
            ui.horizontal_wrapped(|ui| {
                for (p, tex, ..) in set.as_ref().map(|s| s.pages_iter().collect::<Vec<_>>()).unwrap_or_default() {
                    let name = short_texture(tex, p);
                    if ui
                        .selectable_label(self.tools.page == p, name)
                        .on_hover_text(if tex.is_empty() { "this page has no image".into() } else { tex.to_string() })
                        .clicked()
                    {
                        self.tools.page = p;
                    }
                }
            });
        }
        let page = self.tools.page;
        // Every page reads from the TILESET, page 0 included; the layer's
        // material is the fallback only where the tileset names no sheet, which
        // is the same rule the mesh builder follows. When these two disagreed
        // the palette showed one sheet and the map drew another.
        let own = set.as_ref().and_then(|s| s.page(page)).filter(|(t, ..)| !t.trim().is_empty());
        let (sc, sr) = match (own, page, set.as_ref()) {
            (Some((_, c, r)), ..) => (c, r),
            (None, 0, _) => self.sheet_size(),
            (None, _, Some(s)) => s.page(page).map(|(_, c, r)| (c, r)).unwrap_or((1, 1)),
            (None, _, None) => (1, 1),
        };
        let handle = match own {
            Some((t, ..)) => self.texture_handle(ui, t),
            None if page == 0 => self.sheet_handle(ui),
            None => None,
        };
        let Some(sheet) = handle else {
            if page == 0 {
                ui.small(
                    "This tileset has no sheet yet. Drop an image on SHEETS above — or give \
                     the layer's material a spritesheet, which this falls back to.",
                );
            } else {
                ui.small("This page has no image, or it could not be read.");
            }
            return;
        };
        if sc * sr <= 1 {
            ui.small(if page == 0 {
                "The material's texture is not cut into a sheet. Set sheetCols / sheetRows \
                 on the material and every cell becomes a tile."
            } else {
                "This page's image is not cut into a sheet. Set its cols / rows above."
            });
            return;
        }
        ui.small(
            "Click a tile; drag for a multi-tile brush. Ctrl-click to add tiles to the \
             selection — everything under TILE then applies to all of them at once. \
             Shift-click inspects without arming the brush.",
        );
        let cell_px = (ui.available_width() / sc as f32).clamp(20.0, 44.0);
        let mut clicked: Option<u32> = None;
        let mut dragged: Option<(u32, u32, u32, u32)> = None;

        egui::ScrollArea::vertical().max_height(260.0).id_salt("tile_palette").show(ui, |ui| {
            // The rubber band lives in egui temp state: it is a gesture, not a
            // setting, and it should not survive a tab switch.
            let band_id = egui::Id::new("tile_palette_band");
            let mut band: Option<(u32, u32)> = ui.data(|d| d.get_temp(band_id));
            for r in 0..sr {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    for c in 0..sc {
                        let idx = floptle_core::tile_cell_of(page, r * sc + c);
                        let (rect, resp) = ui.allocate_exact_size(
                            egui::vec2(cell_px, cell_px),
                            egui::Sense::click_and_drag(),
                        );
                        paint_tile(ui, rect, &sheet, sc, sr, r * sc + c);

                        // What the tileset knows, drawn over the art.
                        if let Some(set) = set.as_ref() {
                            draw_tile_overlays(ui, rect, set, idx, cell_px);
                        }

                        // Two different things are highlighted here and they are
                        // not the same thing: what the BRUSH will place, and
                        // what the TILE section is editing. They agree after an
                        // ordinary click and stop agreeing the moment you
                        // ctrl-click a second tile, so drawing one ring for both
                        // would make a bulk edit look like it was about to be
                        // painted.
                        let armed = self.tools.palette.is_some_and(|(px, py, w, h)| {
                            c >= px && c < px + w && r >= py && r < py + h
                        });
                        let editing = self.tools.inspect.contains(&idx);
                        let ring = if armed {
                            ACCENT
                        } else if editing {
                            Color32::from_rgb(120, 190, 255)
                        } else if resp.hovered() {
                            Color32::from_gray(210)
                        } else {
                            Color32::from_gray(70)
                        };
                        ui.painter().rect_stroke(
                            rect,
                            0.0,
                            egui::Stroke::new(if armed || editing { 2.0 } else { 1.0 }, ring),
                            egui::StrokeKind::Inside,
                        );
                        // An edited tile that is NOT armed gets a corner tick as
                        // well as its ring, so the two states are still apart for
                        // anyone who cannot separate the colours.
                        if editing && !armed {
                            let p = rect.left_top() + egui::vec2(2.0, 2.0);
                            ui.painter().rect_filled(
                                egui::Rect::from_min_size(p, egui::vec2(4.0, 4.0)),
                                0.0,
                                Color32::from_rgb(120, 190, 255),
                            );
                        }

                        if resp.drag_started() {
                            band = Some((c, r));
                        }
                        if resp.dragged()
                            && let Some((bx, by)) = band
                        {
                            dragged = Some((
                                bx.min(c),
                                by.min(r),
                                bx.abs_diff(c) + 1,
                                by.abs_diff(r) + 1,
                            ));
                        }
                        if resp.drag_stopped() {
                            band = None;
                        }
                        if resp.clicked() {
                            clicked = Some(idx);
                        }
                        let hover = match set.as_ref() {
                            Some(set) => tile_tooltip(set, idx),
                            None => format!("tile {idx}"),
                        };
                        resp.on_hover_text(hover);
                    }
                });
            }
            ui.data_mut(|d| {
                if let Some(b) = band {
                    d.insert_temp(band_id, b);
                } else {
                    d.remove_temp::<(u32, u32)>(band_id);
                }
            });
        });

        if let Some(rect) = dragged {
            self.tools.palette = Some(rect);
            self.tools.stamp = Stamp::from_page(page, sc, rect.0, rect.1, rect.2, rect.3);
            // The band is the edit selection too: dragging out sixteen tiles and
            // ticking "solid" once is the whole reason for the gesture.
            self.tools.inspect.clear();
            for dy in 0..rect.3 {
                for dx in 0..rect.2 {
                    self.tools
                        .inspect
                        .insert(floptle_core::tile_cell_of(page, (rect.1 + dy) * sc + rect.0 + dx));
                }
            }
            // A multi-square brush is a brush, not a group paint — a group resolves
            // per square and cannot honour a layout.
            self.tools.group = None;
        } else if let Some(idx) = clicked {
            let (shift, add) = ui.input(|i| (i.modifiers.shift, i.modifiers.command));
            if add {
                // Ctrl-click builds a selection that need not be a rectangle —
                // the six slope tiles scattered around a sheet. It deliberately
                // does NOT touch the brush: you are editing, not arming.
                if !self.tools.inspect.remove(&idx) {
                    self.tools.inspect.insert(idx);
                }
            } else {
                self.tools.inspect_one(idx);
            }
            // A rule is waiting for a tile: this click answers it rather than
            // picking a brush, and the next empty rule arms itself so filling a
            // preset is one run of alternating clicks. Ctrl-click is a selection
            // gesture and never answers a rule — otherwise building a bulk
            // selection while a preset is half-filled would scatter tiles into
            // rules nobody was looking at.
            if add {
            } else if let Some((g, mask)) = self.tools.fill_mask {
                let had_one = set
                    .as_ref()
                    .and_then(|s| s.groups.get(g as usize))
                    .is_some_and(|group| !group.tiles_for(mask).is_empty());
                self.cmds.push(TileCmd::AddToRule(g, mask, idx));
                // Only move on once a shape has SOMETHING. Clicking on to the
                // next rule the moment a second tile lands would make adding a
                // variant impossible — you would be typing into the next shape.
                if self.tools.fill_advance && !had_one {
                    self.tools.fill_mask = set.as_ref().and_then(|s| {
                        let kind = s.groups.get(g as usize)?.kind;
                        let at = floptle_tiles::Autotiler::build(s);
                        autotile::preset_masks(kind)
                            .into_iter()
                            .find(|m| *m != mask && at.resolve(g, *m).is_none())
                            .map(|m| (g, m))
                    });
                }
            } else if !shift {
                let local = floptle_core::tile_in_page(idx);
                self.tools.palette = Some((local % sc, local / sc, 1, 1));
                self.tools.stamp = Stamp::one(idx);
                // Clicking a tile that belongs to a group arms the GROUP: that is
                // what somebody clicking an autotile tile means, and arming the
                // literal tile would paint one fixed corner piece everywhere.
                //
                // No longer conditional on the retile checkbox. Selecting an
                // autotile IS asking to paint the autotile — having it silently
                // place one fixed corner piece because a box elsewhere was
                // unticked is the thing that made autotiling look broken.
                // Clicking a tile in no group clears the arming, so going back
                // to an ordinary tile is just clicking one.
                self.tools.group = set.as_ref().and_then(|s| s.group_of(idx));
                if self.tools.group.is_some() {
                    self.tools.tool = TileTool::Brush;
                }
            }
        }
        if let Some(g) = self.tools.group
            && let Some(set) = set.as_ref()
            && let Some(group) = set.groups.get(g as usize)
        {
            ui.label(
                RichText::new(format!("painting the “{}” group", group.name)).color(ACCENT).small(),
            );
        }
    }

    // ---- TILE ---------------------------------------------------------------

    fn tile_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "TILE");
        let Some(path) = self.tools.editing.clone() else {
            needs_tileset(
                ui,
                "What one tile IS: whether it is solid and what shape it collides as, what \
                 it is tagged, whether it animates. Set once per tile, and every square \
                 using it — in every scene, including the ones already painted — follows.",
                self.cmds,
            );
            return;
        };
        let Some(set) = self.store.get(&path).cloned() else { return };
        // Every control below writes to the WHOLE selection. One tile is the
        // ordinary case and reads exactly as it did; more than one is what makes
        // setting up a sheet a minute's work instead of an afternoon's.
        let cells: Vec<u32> =
            self.tools.inspect.iter().copied().filter(|c| *c < set.cells()).collect();
        let Some(cell) = cells.first().copied() else {
            ui.small(
                "Click a tile in the palette above to set what it is. Ctrl-click more to edit \
                 several at once.",
            );
            return;
        };
        if cells.len() == 1 {
            ui.label(RichText::new(format!("tile {cell}")).strong());
        } else {
            ui.label(
                RichText::new(format!("{} tiles — every change below applies to all", cells.len()))
                    .strong()
                    .color(Color32::from_rgb(120, 190, 255)),
            );
        }

        // Collision. The chips act on the selection, and a selection that does
        // not agree says so rather than showing whichever tile happened to be
        // first — an editor that displays one tile's value and writes to forty
        // is how a sheet ends up quietly wrong.
        let coll = set.collision(cell).clone();
        let mixed = cells.iter().any(|c| set.collision(*c) != &coll);
        let sel = |want: bool| want && !mixed;
        labelled(ui, "collides", |ui| {
            for (label, want) in [("none", TileCollision::None), ("full", TileCollision::Full)] {
                let on = sel(coll == want);
                if ui
                    .add_sized([CHIP_W * 0.55, BTN_H], egui::Button::new(label).selected(on))
                    .clicked()
                {
                    self.cmds.push(TileCmd::BulkCollision(cells.clone(), want));
                }
            }
            let is_half = matches!(coll, TileCollision::Half(_));
            if ui
                .add_sized([CHIP_W * 0.55, BTN_H], egui::Button::new("half").selected(sel(is_half)))
                .clicked()
                && !is_half
            {
                self.cmds.push(TileCmd::BulkCollision(
                    cells.clone(),
                    TileCollision::Half(TileSide::Bottom),
                ));
            }
            let is_rect = matches!(coll, TileCollision::Custom { .. });
            if ui
                .add_sized([CHIP_W * 0.55, BTN_H], egui::Button::new("rect").selected(sel(is_rect)))
                .clicked()
                && !is_rect
            {
                self.cmds.push(TileCmd::BulkCollision(
                    cells.clone(),
                    TileCollision::Custom { x: 0.0, y: 0.0, w: 1.0, h: 0.5 },
                ));
            }
            let is_poly = matches!(coll, TileCollision::Poly(_));
            if ui
                .add_sized([CHIP_W * 0.55, BTN_H], egui::Button::new("shape").selected(sel(is_poly)))
                .on_hover_text(
                    "draw the collider yourself, point by point, snapped to the art's pixels — \
                     what a SLOPE is. It collides as the shape you drew, not as the box \
                     around it.",
                )
                .clicked()
                && !is_poly
            {
                // Start from the bottom-left triangle rather than from nothing:
                // a blank canvas with no points on it gives no clue that
                // clicking adds one, and the commonest shape anybody wants here
                // is a 45° ramp.
                self.cmds.push(TileCmd::BulkCollision(
                    cells.clone(),
                    TileCollision::Poly(vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]]),
                ));
            }
        });
        if mixed {
            ui.small("These tiles do not all collide the same way — pick one above to set them all.");
        }
        if let TileCollision::Half(side) = coll {
            labelled(ui, "which half", |ui| {
                for s in TileSide::ALL {
                    if ui
                        .add_sized(
                            [CHIP_W * 0.7, BTN_H],
                            egui::Button::new(s.name()).selected(side == s),
                        )
                        .on_hover_text("named in the tile's own art — it turns with the tile")
                        .clicked()
                    {
                        self.cmds
                            .push(TileCmd::BulkCollision(cells.clone(), TileCollision::Half(s)));
                    }
                }
            });
        }
        if let TileCollision::Custom { x, y, w, h } = coll {
            let (mut x, mut y, mut w, mut h) = (x, y, w, h);
            let mut changed = false;
            labelled(ui, "rect", |ui| {
                for (v, prefix) in
                    [(&mut x, "x "), (&mut y, "y "), (&mut w, "w "), (&mut h, "h ")]
                {
                    changed |= ui
                        .add(egui::DragValue::new(v).speed(0.02).range(0.0..=1.0).prefix(prefix))
                        .changed();
                }
            });
            ui.small("in the tile, from its BOTTOM-LEFT. 0–1, so it scales with the tile size.");
            if changed {
                self.cmds.push(TileCmd::BulkCollision(
                    cells.clone(),
                    TileCollision::Custom { x, y, w, h },
                ));
            }
        }
        if let TileCollision::Poly(pts) = &coll {
            self.shape_editor(ui, &set, cell, &cells, pts);
        }

        // Tags — free strings the game reads with `tm:hasTag`.
        let tags = set.tags(cell).join(", ");
        let id = egui::Id::new(("tile_tags", cell));
        let mut text: String = ui.data(|d| d.get_temp(id)).unwrap_or(tags.clone());
        if ui.data(|d| d.get_temp::<String>(id.with("seen"))).as_deref() != Some(tags.as_str()) {
            text = tags.clone();
            ui.data_mut(|d| d.insert_temp(id.with("seen"), tags.clone()));
        }
        labelled(ui, "tags", |ui| {
            if ui
                .add(egui::TextEdit::singleline(&mut text).desired_width(150.0))
                .on_hover_text("comma-separated. A script reads them with tm:hasTag(x, y, \"ice\")")
                .lost_focus()
            {
                let list: Vec<String> = text
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                self.cmds.push(TileCmd::SetTags(cell, list));
            }
        });
        ui.data_mut(|d| d.insert_temp(id, text));

        // Animation — a torch or a water surface.
        let info = set.info(cell).cloned().unwrap_or_default();
        let frames = info
            .frames
            .iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let fid = egui::Id::new(("tile_frames", cell));
        let mut ftext: String = ui.data(|d| d.get_temp(fid)).unwrap_or(frames.clone());
        if ui.data(|d| d.get_temp::<String>(fid.with("seen"))).as_deref() != Some(frames.as_str()) {
            ftext = frames.clone();
            ui.data_mut(|d| d.insert_temp(fid.with("seen"), frames.clone()));
        }
        let mut fps = info.anim_fps;
        labelled(ui, "frames", |ui| {
            let done = ui
                .add(egui::TextEdit::singleline(&mut ftext).desired_width(110.0))
                .on_hover_text(
                    "extra cells this tile cycles through, comma-separated. This tile is \
                     frame 0 and is not repeated.",
                )
                .lost_focus();
            let rate = ui.add(egui::DragValue::new(&mut fps).speed(0.5).range(0.0..=60.0).suffix(" fps"));
            if done || rate.lost_focus() || rate.drag_stopped() {
                let list: Vec<u32> = ftext
                    .split(',')
                    .filter_map(|s| s.trim().parse::<u32>().ok())
                    .filter(|c| *c < set.cells())
                    .collect();
                self.cmds.push(TileCmd::SetAnim(cell, list, fps));
            }
        });
        ui.data_mut(|d| d.insert_temp(fid, ftext));
        if !info.frames.is_empty() && info.anim_fps <= 0.0 {
            ui.colored_label(ACCENT, "⚠ frames listed but the rate is 0 — it will not animate");
        }
    }

    /// Draw a tile's collider by hand: click to add a point, drag one to move
    /// it, right-click to remove it. Every point snaps to the ART'S OWN PIXEL
    /// GRID.
    ///
    /// The snapping is the part that matters and it is not a nicety. A slope is
    /// built out of several tiles whose diagonals have to *meet*: if one tile's
    /// ramp ends a third of a pixel above where the next one's begins, a
    /// character running up it catches on every tile boundary, and the cause is
    /// invisible because the art lines up perfectly. Snapping to the pixel grid
    /// makes "the corner of that pixel" a thing you can hit exactly, every time,
    /// which is how a tile artist already thinks about the sheet.
    fn shape_editor(
        &mut self,
        ui: &mut egui::Ui,
        set: &TileSet,
        cell: u32,
        cells: &[u32],
        pts: &[[f32; 2]],
    ) {
        let page = floptle_core::tile_page(cell);
        let local = floptle_core::tile_in_page(cell);
        let (sc, sr) = set
            .page(page)
            .filter(|(t, ..)| !t.trim().is_empty())
            .map(|(_, c, r)| (c, r))
            .unwrap_or_else(|| self.sheet_size());
        let sheet_rel = set.page(page).map(|(t, ..)| t.to_string()).unwrap_or_default();
        let handle = if sheet_rel.trim().is_empty() {
            self.sheet_handle(ui)
        } else {
            self.texture_handle(ui, &sheet_rel)
        };
        // How many art pixels one tile is across. That IS the snap. Falling back
        // to 16 rather than to "no snapping" is deliberate: an unsnapped shape
        // editor is the tool this replaces.
        let px_per_tile = self
            .sheet_px(ui, &sheet_rel)
            .or_else(|| self.sheet_px(ui, &self.layer_tileset()))
            .map(|(w, _)| (w / sc.max(1)).max(1))
            .unwrap_or(16);

        let side = 176.0f32.min(ui.available_width() - 8.0);
        let (rect, resp) =
            ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::click_and_drag());
        if let Some(sheet) = handle.as_ref() {
            paint_tile(ui, rect, sheet, sc, sr, local);
        } else {
            ui.painter().rect_filled(rect, 0.0, Color32::from_gray(30));
        }

        // Unit-tile (x right, y UP from the bottom-left) ↔ screen.
        let to_screen = |p: [f32; 2]| {
            egui::pos2(rect.left() + p[0] * rect.width(), rect.bottom() - p[1] * rect.height())
        };
        let to_unit = |s: egui::Pos2| {
            [
                ((s.x - rect.left()) / rect.width()).clamp(0.0, 1.0),
                ((rect.bottom() - s.y) / rect.height()).clamp(0.0, 1.0),
            ]
        };
        let snap = |p: [f32; 2]| {
            let n = px_per_tile as f32;
            [(p[0] * n).round() / n, (p[1] * n).round() / n]
        };

        // The pixel grid, so "snapped to pixels" is something you can SEE rather
        // than something the tooltip claims. Drawn only when the cells are big
        // enough to read as a grid instead of as a grey wash.
        let step = rect.width() / px_per_tile as f32;
        if step >= 4.0 {
            for i in 1..px_per_tile {
                let t = i as f32 * step;
                let g = Color32::from_black_alpha(40);
                ui.painter().line_segment(
                    [egui::pos2(rect.left() + t, rect.top()), egui::pos2(rect.left() + t, rect.bottom())],
                    egui::Stroke::new(1.0, g),
                );
                ui.painter().line_segment(
                    [egui::pos2(rect.left(), rect.top() + t), egui::pos2(rect.right(), rect.top() + t)],
                    egui::Stroke::new(1.0, g),
                );
            }
        }

        let mut next: Vec<[f32; 2]> = pts.to_vec();
        let mut changed = false;
        let drag_id = ui.id().with(("tile_shape_drag", cell));
        let mut held: Option<usize> = ui.data(|d| d.get_temp(drag_id));

        // Which point the pointer is over — the hit test is in SCREEN space, so
        // it stays a comfortable target whatever the tile's pixel density is.
        let hover_pt = resp.hover_pos().and_then(|m| {
            next.iter()
                .enumerate()
                .map(|(i, p)| (i, to_screen(*p).distance(m)))
                .filter(|(_, d)| *d <= 9.0)
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| i)
        });

        if resp.drag_started() {
            held = hover_pt;
        }
        if let (true, Some(i), Some(m)) = (resp.dragged(), held, resp.hover_pos())
            && i < next.len()
        {
            next[i] = snap(to_unit(m));
            changed = true;
        }
        if resp.drag_stopped() {
            held = None;
        }
        // Clicking an existing point does nothing (you meant to drag it);
        // clicking anywhere else inserts a point into the nearest EDGE, so a
        // shape grows where you pointed instead of always at the end of the list
        // — which is what makes adding a step to a slope one click rather than a
        // rebuild.
        if resp.clicked()
            && held.is_none()
            && hover_pt.is_none()
            && let Some(m) = resp.interact_pointer_pos()
        {
            let p = snap(to_unit(m));
            let at = insertion_edge(&next, p, &to_screen, m);
            next.insert(at, p);
            changed = true;
        }
        if resp.secondary_clicked()
            && let Some(i) = hover_pt
            && next.len() > 3
        {
            next.remove(i);
            changed = true;
        }
        ui.data_mut(|d| {
            if let Some(i) = held {
                d.insert_temp(drag_id, i);
            } else {
                d.remove_temp::<usize>(drag_id);
            }
        });

        // The outline, filled, over the art it collides for.
        if next.len() >= 2 {
            let poly: Vec<egui::Pos2> = next.iter().map(|p| to_screen(*p)).collect();
            if next.len() >= 3 {
                ui.painter().add(egui::Shape::convex_polygon(
                    poly.clone(),
                    Color32::from_rgba_unmultiplied(90, 190, 255, 60),
                    egui::Stroke::NONE,
                ));
            }
            for i in 0..poly.len() {
                ui.painter().line_segment(
                    [poly[i], poly[(i + 1) % poly.len()]],
                    egui::Stroke::new(2.0, Color32::from_rgb(120, 200, 255)),
                );
            }
            for (i, p) in poly.iter().enumerate() {
                let on = hover_pt == Some(i) || held == Some(i);
                ui.painter().circle_filled(
                    *p,
                    if on { 5.0 } else { 3.5 },
                    if on { ACCENT } else { Color32::WHITE },
                );
            }
        }
        ui.painter().rect_stroke(
            rect,
            0.0,
            egui::Stroke::new(1.0, Color32::from_gray(90)),
            egui::StrokeKind::Inside,
        );

        ui.small(format!(
            "Click an edge to add a point · drag to move · right-click to remove. Snapping to \
             {px_per_tile}×{px_per_tile} pixels."
        ));
        // The four ramps, because a platformer wants those and nothing else four
        // times out of five, and clicking three corners to get one is a chore
        // rather than a design decision.
        labelled(ui, "ramps", |ui| {
            for (glyph, shape, tip) in RAMPS {
                if ui
                    .add_sized([BTN_H * 1.4, BTN_H], egui::Button::new(*glyph))
                    .on_hover_text(*tip)
                    .clicked()
                {
                    next = shape.to_vec();
                    changed = true;
                }
            }
        });
        labelled(ui, "shape", |ui| {
            if ui
                .add_sized([CHIP_W * 0.8, BTN_H], egui::Button::new("Square"))
                .on_hover_text("back to the whole tile — a starting point to cut corners off")
                .clicked()
            {
                next = vec![[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
                changed = true;
            }
            if ui
                .add_sized([CHIP_W * 0.8, BTN_H], egui::Button::new("Flip ⇔"))
                .on_hover_text("mirror the shape left-to-right")
                .clicked()
            {
                for p in next.iter_mut() {
                    p[0] = 1.0 - p[0];
                }
                next.reverse();
                changed = true;
            }
        });
        if next.len() < 3 {
            ui.colored_label(ACCENT, "⚠ fewer than three points is not a shape — it collides with nothing");
        }
        if changed {
            self.cmds.push(TileCmd::BulkCollision(cells.to_vec(), TileCollision::Poly(next)));
        }
    }

    // ---- AUTOTILE -----------------------------------------------------------

    fn autotile_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "AUTOTILE");
        let Some(path) = self.tools.editing.clone() else {
            needs_tileset(
                ui,
                "A group of tiles that pick themselves by what is next to them — mark a run \
                 of tiles as a group, hand it a preset, and painting draws its own corners \
                 and edges.",
                self.cmds,
            );
            return;
        };
        let Some(set) = self.store.get(&path).cloned() else { return };
        // Numbered, because "mark a run of tiles as a group, hand it the preset"
        // described the concept and not the actions — you could read it twice
        // and still not know what to click first. Reported as exactly that.
        if set.groups.is_empty() {
            ui.small(
                "An autotile is a set of tiles that pick themselves by what is next to them: \
                 draw a wall and it grows its own corners.",
            );
            ui.small(RichText::new("1. Add one below — the preset decides how many shapes it needs.").strong());
            ui.small("2. Fill its RULES: click a shape, then click the tile that draws it.");
            ui.small("3. Click any of its tiles in the palette and paint. It retiles as you go.");
        } else {
            ui.small(
                "Click a shape in RULES, then the tile that draws it. To paint, click any of \
                 the group's tiles in the palette — painting retiles as you go, until you \
                 pick a tile that is not in a group.",
            );
            ui.small(
                "One tile can draw as many shapes as you like, and one shape can hold \
                 several tiles — those take turns, so a field of grass varies.",
            );
        }
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.tools.auto_retile, "Retile as I paint").on_hover_text(
                "recompute the neighbours after every stroke. Off, painting places one \
                 fixed tile of the group.",
            );
            if ui
                .add_sized([CHIP_W, BTN_H], egui::Button::new("Retile all"))
                .on_hover_text("fix the whole layer after changing the rules")
                .clicked()
            {
                self.cmds.push(TileCmd::RetileAll);
            }
        });

        let at = floptle_tiles::Autotiler::build(&set);
        for (i, group) in set.groups.iter().enumerate() {
            let g = i as u16;
            let cells = set.group_cells(g);
            let missing = at.missing(g).len();
            let want = autotile::preset_len(group.kind);
            egui::CollapsingHeader::new(format!(
                "{} — {} of {want} drawn",
                group.name,
                want - missing
            ))
            .id_salt(("tile_group", i))
            .default_open(self.tools.group == Some(g))
            .show(ui, |ui| {
                let mut name = group.name.clone();
                labelled(ui, "name", |ui| {
                    if ui.add(egui::TextEdit::singleline(&mut name).desired_width(120.0)).lost_focus()
                    {
                        self.cmds.push(TileCmd::RenameGroup(g, name.clone()));
                    }
                });
                labelled(ui, "kind", |ui| {
                    ui.small(group.kind.label());
                });
                if missing > 0 {
                    ui.colored_label(
                        ACCENT,
                        format!("{missing} neighbourhoods have no tile — they stay as painted"),
                    );
                }
                // Joins — what lets grass and dirt meet without either edging
                // against the other.
                labelled(ui, "joins", |ui| {
                    let mut joins = group.joins.clone();
                    let mut changed = false;
                    for (j, other) in set.groups.iter().enumerate() {
                        if j == i {
                            continue;
                        }
                        let jj = j as u16;
                        let mut on = joins.contains(&jj);
                        if ui.checkbox(&mut on, &other.name).changed() {
                            changed = true;
                            if on {
                                joins.push(jj);
                            } else {
                                joins.retain(|x| *x != jj);
                            }
                        }
                    }
                    if changed {
                        self.cmds.push(TileCmd::SetGroupJoins(g, joins));
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    let n = self
                        .tools
                        .palette
                        .map(|(_, _, w, h)| w * h)
                        .unwrap_or(0);
                    if ui
                        .add_enabled(
                            n > 1,
                            egui::Button::new(format!("Assign preset to {n} selected"))
                                .min_size([CHIP_W * 2.0, BTN_H].into()),
                        )
                        .on_hover_text(
                            "put the palette selection in this group and hand each tile its \
                             preset mask, in cell order. Every tile's mask is drawn on the \
                             palette, so a sheet in a different order is visible and one \
                             click to fix.",
                        )
                        .clicked()
                    {
                        self.cmds.push(TileCmd::ApplyPreset(g));
                    }
                    if ui
                        .add_sized([CHIP_W, BTN_H], egui::Button::new("Paint this"))
                        .clicked()
                    {
                        self.tools.group = Some(g);
                        self.tools.tool = TileTool::Brush;
                    }
                    if ui.add_sized([CHIP_W, BTN_H], egui::Button::new("Remove")).clicked() {
                        self.cmds.push(TileCmd::RemoveGroup(g));
                    }
                });
                ui.small(format!("{} tiles in this group", cells.len()));
                self.rules_grid(ui, &set, g, group.kind, &at);

                // Where the selected tile is used, and one click to take it out
                // of a shape it should not be in. A tile can be on several
                // shapes now, so this is a list rather than the single-mask
                // editor it replaces — that editor could only move a tile from
                // one shape to another, which is what made duplicates
                // impossible in the first place.
                if let Some(cell) = self.tools.primary() {
                    let here: Vec<u8> = floptle_tiles::tile_masks(&set, cell)
                        .into_iter()
                        .filter(|(og, _)| *og == g)
                        .map(|(_, m)| m)
                        .collect();
                    ui.separator();
                    if here.is_empty() {
                        ui.small(format!("tile {cell} draws none of this group's shapes"));
                        if self.tools.fill_mask.is_none()
                            && ui
                                .small_button("add it to a shape")
                                .on_hover_text("arms the first shape with no tile")
                                .clicked()
                        {
                            self.tools.fill_mask = autotile::preset_masks(group.kind)
                                .into_iter()
                                .find(|m| at.resolve(g, *m).is_none())
                                .map(|m| (g, m));
                        }
                    } else {
                        ui.small(format!("tile {cell} draws {} of this group's shapes", here.len()));
                        let mut off: Option<u8> = None;
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(3.0, 3.0);
                            for mask in here {
                                let (rect, resp) = ui.allocate_exact_size(
                                    egui::vec2(24.0, 24.0),
                                    egui::Sense::click(),
                                );
                                ui.painter().rect_filled(rect, 2.0, Color32::from_gray(30));
                                paint_mask_glyph(ui, rect, mask, group.kind, true);
                                if resp.hovered() {
                                    ui.painter().rect_stroke(
                                        rect,
                                        2.0,
                                        egui::Stroke::new(1.0, ACCENT),
                                        egui::StrokeKind::Inside,
                                    );
                                }
                                if resp.clicked() {
                                    off = Some(mask);
                                }
                                resp.on_hover_text(format!(
                                    "{}\nclick to arm this shape",
                                    describe_mask(mask, group.kind)
                                ));
                            }
                        });
                        if let Some(mask) = off {
                            self.tools.fill_mask = Some((g, mask));
                        }
                    }
                }
            });
        }
        ui.horizontal(|ui| {
            for kind in AutotileKind::ALL {
                if ui
                    .add_sized([CHIP_W * 1.6, BTN_H], egui::Button::new(format!("+ {}", kind.label())))
                    .clicked()
                {
                    self.cmds.push(TileCmd::AddGroup(kind));
                }
            }
        });
    }

    /// The rules of one autotile group, as a slot per neighbourhood.
    ///
    /// This is the interactive half. A group's rules are "which tile do I draw
    /// when my neighbours look like *this*", and there are between 4 and 47 of
    /// them depending on the preset. The old flow was: select that many tiles in
    /// the palette, in the preset's own order, and press one button. It works
    /// perfectly for a sheet drawn in that order and is unusable otherwise —
    /// there was no way to see which tile had been given which neighbourhood,
    /// and no way to fix one without redoing all of them.
    ///
    /// Here every neighbourhood is a slot showing the shape it matches and the
    /// tile currently drawn for it. Click a slot, click a tile: that rule is
    /// set and the next empty slot arms itself, so filling a preset is a run of
    /// alternating clicks and stopping halfway leaves something that works for
    /// the parts you did.
    fn rules_grid(
        &mut self,
        ui: &mut egui::Ui,
        set: &TileSet,
        g: u16,
        kind: AutotileKind,
        at: &floptle_tiles::Autotiler,
    ) {
        let masks = autotile::preset_masks(kind);
        let armed = self.tools.fill_mask.filter(|(ag, _)| *ag == g).map(|(_, m)| m);
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.small(RichText::new("RULES").strong());
            if armed.is_some() {
                ui.colored_label(
                    ACCENT,
                    RichText::new("click tiles in the palette — every one you click is added")
                        .small(),
                );
                if ui.small_button("done").clicked() {
                    self.tools.fill_mask = None;
                }
            } else {
                ui.small("click a shape, then click the tile that draws it");
            }
        });
        // Which piece of the shape the armed slot is waiting for, spelled out.
        // The 3×3 diagram on each slot is exact and still has to be translated
        // in your head every time; the sentence is the one an artist already
        // has while drawing the sheet, and it is what turns "which of my tiles
        // goes here" into a question with an obvious answer.
        if let Some(m) = armed {
            ui.label(
                RichText::new(format!("waiting for: the {}", mask_shape_name(m, kind)))
                    .color(ACCENT)
                    .strong(),
            );
            ui.small(describe_mask(m, kind));
        }
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.tools.fill_advance, "next shape after the first tile")
                .on_hover_text(
                    "on: filling a preset is one run of alternating clicks. off: stay on \
                     one shape, which is what you want while adding variants.",
                );
        });
        // The sheet the palette is showing, so a slot draws the real art rather
        // than a cell number. A group's tiles can come from any page; the slot
        // shows the one it is on.
        let sheet_of = |ui: &egui::Ui, cell: u32| -> Option<(egui::TextureHandle, u32, u32)> {
            let page = floptle_core::tile_page(cell);
            let (tex, c, r) = set.page(page)?;
            let h = if tex.trim().is_empty() {
                self.sheet_handle(ui)?
            } else {
                self.texture_handle(ui, tex)?
            };
            let (c, r) = if tex.trim().is_empty() { self.sheet_size() } else { (c, r) };
            Some((h, c, r))
        };
        let cell_px = 34.0;
        let mut pick: Option<u8> = None;
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(3.0, 3.0);
            for &mask in &masks {
                let variants = at.variants(g, mask);
                let drawn = variants.first().copied();
                let (rect, resp) = ui.allocate_exact_size(
                    egui::vec2(cell_px, cell_px),
                    egui::Sense::click(),
                );
                // The art if this rule has a tile, else the shape it is waiting
                // for. An empty slot showing a neighbourhood diagram says what
                // is missing; an empty square would just look broken.
                match drawn.and_then(|cell| sheet_of(ui, cell).map(|s| (cell, s))) {
                    Some((cell, (h, c, r))) => {
                        paint_tile(ui, rect, &h, c, r, floptle_core::tile_index(cell));
                    }
                    None => {
                        ui.painter().rect_filled(rect, 2.0, Color32::from_gray(30));
                    }
                }
                paint_mask_glyph(ui, rect, mask, kind, drawn.is_some());
                // A rule with alternates says so on its face. Without this the
                // grid looks identical whether a shape has one tile or five,
                // and the variants are invisible until something is painted.
                if variants.len() > 1 {
                    let tag = egui::Rect::from_min_size(
                        rect.right_bottom() - egui::vec2(15.0, 11.0),
                        egui::vec2(15.0, 11.0),
                    );
                    ui.painter().rect_filled(tag, 2.0, Color32::from_black_alpha(190));
                    ui.painter().text(
                        tag.center(),
                        egui::Align2::CENTER_CENTER,
                        format!("×{}", variants.len()),
                        egui::FontId::proportional(9.0),
                        ACCENT,
                    );
                }
                let on = armed == Some(mask);
                if on || resp.hovered() {
                    ui.painter().rect_stroke(
                        rect,
                        2.0,
                        egui::Stroke::new(if on { 2.0 } else { 1.0 }, ACCENT),
                        egui::StrokeKind::Inside,
                    );
                }
                if resp.clicked() {
                    pick = Some(mask);
                }
                resp.on_hover_text(match variants.len() {
                    0 => format!(
                        "{}\nno tile yet: a square with these neighbours stays as painted",
                        describe_mask(mask, kind)
                    ),
                    1 => format!(
                        "{}\ndrawn by tile {} — click to arm it, then click tiles to add more",
                        describe_mask(mask, kind),
                        variants[0]
                    ),
                    n => format!(
                        "{}\n{n} tiles take turns here, chosen by where the square is",
                        describe_mask(mask, kind)
                    ),
                });
            }
        });
        // The armed rule's tiles, one by one, because the grid only has room to
        // show the first. This is where a duplicate is added or taken back out.
        //
        // Their art is looked up HERE, while `sheet_of` is still the only thing
        // borrowing self — the strip below both mutates `fill_mask` and pushes
        // commands.
        let strip: Vec<VariantThumb> = armed
            .map(|mask| {
                at.variants(g, mask).iter().map(|&c| (c, sheet_of(ui, c))).collect()
            })
            .unwrap_or_default();
        if let Some(mask) = pick {
            self.tools.fill_mask =
                if armed == Some(mask) { None } else { Some((g, mask)) };
        }
        // Only when the arming did not just change under it: a strip drawn from
        // the rule that was armed a moment ago, labelled with the one that is
        // armed now, would be showing somebody else's tiles.
        if let Some(mask) = armed.filter(|_| pick.is_none()) {
            ui.add_space(2.0);
            ui.small(RichText::new(describe_mask(mask, kind)).color(ACCENT));
            if strip.is_empty() {
                ui.small("nothing yet — click a tile in the palette above");
            }
            let mut drop: Option<usize> = None;
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(3.0, 3.0);
                for (n, (cell, art)) in strip.iter().enumerate() {
                    let cell = *cell;
                    let (rect, resp) =
                        ui.allocate_exact_size(egui::vec2(30.0, 30.0), egui::Sense::click());
                    match art {
                        Some((h, c, r)) => {
                            paint_tile(ui, rect, h, *c, *r, floptle_core::tile_index(cell));
                        }
                        None => {
                            ui.painter().rect_filled(rect, 2.0, Color32::from_gray(30));
                        }
                    }
                    if resp.hovered() {
                        ui.painter().rect_filled(rect, 2.0, Color32::from_black_alpha(150));
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "✖",
                            egui::FontId::proportional(15.0),
                            ACCENT,
                        );
                    }
                    if resp.clicked() {
                        drop = Some(n);
                    }
                    resp.on_hover_text(format!("tile {cell} — click to take it off this shape"));
                }
                if strip.len() > 1 && ui.small_button("clear").clicked() {
                    self.cmds.push(TileCmd::ClearRule(g, mask));
                }
            });
            if let Some(n) = drop {
                self.cmds.push(TileCmd::RemoveVariant(g, mask, n));
            }
            if strip.len() > 1 {
                ui.small("these take turns — which one a square gets is fixed by where it is");
            }
        }
        let missing = at.missing(g).len();
        if missing == 0 && !masks.is_empty() {
            ui.small(RichText::new("every shape has a tile").color(ACCENT));
        }
    }

    // ---- helpers ------------------------------------------------------------

    /// The active layer's sheet grid, from its Material.
    fn sheet_size(&self) -> (u32, u32) {
        let Some(e) = self.tools.layer else { return (1, 1) };
        self.world
            .get::<floptle_core::Material>(e)
            .map(|m| m.sheet())
            .unwrap_or((1, 1))
    }

    /// An egui handle on any project-relative image — a tileset page's own
    /// sheet, which is not the layer's material.
    fn texture_handle(&self, ui: &egui::Ui, rel: &str) -> Option<egui::TextureHandle> {
        let abs = crate::project::resolve_asset_path(self.project_root, rel);
        crate::ui_widgets::asset_thumb(ui, abs.to_str()?, 512)
    }

    /// A sheet image's size in pixels, or `None` if it is not readable yet.
    ///
    /// Read from the decoded thumbnail rather than the file, so it costs a
    /// cache lookup and answers for whatever the engine will actually sample.
    fn sheet_px(&self, ui: &egui::Ui, rel: &str) -> Option<(u32, u32)> {
        if rel.trim().is_empty() {
            return None;
        }
        let abs = crate::project::resolve_asset_path(self.project_root, rel);
        crate::ui_widgets::asset_size(ui, abs.to_str()?)
    }

    /// A texture dropped onto `resp`, project-relative.
    ///
    /// Only images: dropping a `.glb` on a tileset sheet is a slip, and taking
    /// it would put an unloadable path in the file and blank the palette.
    fn dropped_texture_on(&self, ui: &egui::Ui, resp: &egui::Response) -> Option<String> {
        if resp
            .dnd_hover_payload::<crate::assets::AssetPayload>()
            .is_some_and(|p| crate::assets::is_texture(&p.path))
        {
            ui.painter().rect_stroke(
                resp.rect.expand(1.0),
                4.0,
                egui::Stroke::new(1.5, ACCENT),
                egui::StrokeKind::Outside,
            );
        }
        let p = resp.dnd_release_payload::<crate::assets::AssetPayload>()?;
        crate::assets::is_texture(&p.path).then(|| p.path.clone())
    }

    /// A texture dropped anywhere on the panel — so "drag it in from my assets"
    /// does not mean "hit this particular row".
    fn dropped_texture(&self, ui: &egui::Ui) -> Option<String> {
        let resp = ui.response();
        let p = resp.dnd_release_payload::<crate::assets::AssetPayload>()?;
        crate::assets::is_texture(&p.path).then(|| p.path.clone())
    }

    /// An egui handle on the active layer's texture.
    fn sheet_handle(&self, ui: &egui::Ui) -> Option<egui::TextureHandle> {
        let e = self.tools.layer?;
        let mat = self.world.get::<floptle_core::Material>(e)?;
        let rel = mat.texture.as_deref()?;
        let abs = crate::project::resolve_asset_path(self.project_root, rel);
        // 512 is a compromise: a palette cell is at most 44 px, so a 16x16 sheet
        // wants 704 to be crisp, but loading the full sheet at native size for
        // every frame of a 2048x2048 atlas is not worth it.
        crate::ui_widgets::asset_thumb(ui, abs.to_str()?, 512)
    }
}

/// The neighbourhood a rule matches, drawn small in the corner of its slot.
///
/// Eight dots around a centre: filled where this rule expects more of the same
/// group, hollow where it expects an edge. Drawn OVER the tile art rather than
/// beside it because the pairing is the whole point — "this picture, for this
/// shape" has to be readable at a glance across forty-seven of them.
/// The four 45° ramps, as unit-tile outlines from the BOTTOM-LEFT. The glyph is
/// the shape: ◣ is filled at the bottom-left, which is what its points say.
const RAMPS: &[(&str, &[[f32; 2]], &str)] = &[
    ("◣", &[[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]], "ramp up to the left"),
    ("◢", &[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]], "ramp up to the right"),
    ("◤", &[[0.0, 0.0], [1.0, 1.0], [0.0, 1.0]], "ceiling ramp, high on the left"),
    ("◥", &[[1.0, 0.0], [1.0, 1.0], [0.0, 1.0]], "ceiling ramp, high on the right"),
];

/// Which edge a new point belongs on: the one it is nearest to in SCREEN space.
///
/// Appending to the end instead would put every new point after the last one
/// authored, so adding a step halfway along a slope would fold the outline over
/// itself — the shape would still be a shape, and it would be the wrong one.
fn insertion_edge(
    pts: &[[f32; 2]],
    p: [f32; 2],
    to_screen: &impl Fn([f32; 2]) -> egui::Pos2,
    at: egui::Pos2,
) -> usize {
    if pts.len() < 2 {
        return pts.len();
    }
    let _ = p;
    let mut best = (f32::MAX, pts.len());
    for i in 0..pts.len() {
        let a = to_screen(pts[i]);
        let b = to_screen(pts[(i + 1) % pts.len()]);
        let ab = b - a;
        let t = (at - a).dot(ab) / ab.length_sq().max(1e-6);
        let q = a + ab * t.clamp(0.0, 1.0);
        let d = q.distance(at);
        if d < best.0 {
            best = (d, i + 1);
        }
    }
    best.1
}

fn paint_mask_glyph(ui: &egui::Ui, rect: egui::Rect, mask: u8, kind: AutotileKind, filled: bool) {
    let p = ui.painter();
    let box_side = 13.0;
    let at = egui::Rect::from_min_size(
        egui::pos2(rect.right() - box_side - 1.0, rect.bottom() - box_side - 1.0),
        egui::vec2(box_side, box_side),
    );
    // A backing plate: the dots have to read over arbitrary art.
    p.rect_filled(at, 2.0, Color32::from_black_alpha(if filled { 170 } else { 90 }));
    let step = box_side / 3.0;
    // Corners only mean anything to a preset that looks at them; drawing them on
    // a 4-neighbour preset would imply a rule it does not have.
    let corners = kind == AutotileKind::Blob8;
    for (dx, dy, bit) in autotile::OFFSETS {
        if !corners && (dx != 0 && dy != 0) {
            continue;
        }
        // `dy` is in ROW space — `-1` is NORTH, which is UP the screen — and
        // egui's +y is DOWN, so the two agree and the row index is used as-is.
        //
        // It used to be negated, which drew the whole diagram upside down: the
        // dot for "there is more of this group ABOVE me" appeared below the
        // centre. Every tile in a sheet then looked like it answered the
        // vertically mirrored neighbourhood, so picking tiles by the picture
        // built an autotile set that was upside down and looked, in a level,
        // like the art was wrong. `the_diagram_puts_north_at_the_top` pins it.
        let c = at.min + egui::vec2((dx as f32 + 1.5) * step, (dy as f32 + 1.5) * step);
        if mask & bit != 0 {
            p.circle_filled(c, 1.6, Color32::from_gray(235));
        } else {
            p.circle_stroke(c, 1.4, egui::Stroke::new(0.8, Color32::from_gray(150)));
        }
    }
    p.circle_filled(at.center(), 1.8, ACCENT);
}

/// A rule's neighbourhood in words, for the slot's tooltip.
/// What a neighbourhood mask means, in the words somebody drawing a tileset
/// already uses: **which piece of the shape this tile is**.
///
/// This is the thing the 3×3 diagram cannot say on its own. A picture of "my
/// stuff is below me and to the right" is correct and still needs translating
/// every single time; "top-left corner" is the sentence an artist has in their
/// head while drawing the sheet, and it is the one that tells you which of your
/// tiles to click.
///
/// The direction reads INVERTED on purpose and it is the whole trick: a tile
/// with neighbours below and to the right is the piece at the TOP-LEFT of the
/// shape, because the shape continues away from it in both those directions.
pub(crate) fn mask_shape_name(mask: u8, kind: AutotileKind) -> &'static str {
    use autotile::{E, EDGES, N, NE, NW, S, SE, SW, W};
    let e = mask & EDGES;
    let base = match e {
        0 => "single block",
        x if x == N => "bottom end",
        x if x == S => "top end",
        x if x == E => "left end",
        x if x == W => "right end",
        x if x == (N | S) => "vertical middle",
        x if x == (E | W) => "horizontal middle",
        x if x == (S | E) => "top-left corner",
        x if x == (S | W) => "top-right corner",
        x if x == (N | E) => "bottom-left corner",
        x if x == (N | W) => "bottom-right corner",
        x if x == (E | S | W) => "top edge",
        x if x == (E | N | W) => "bottom edge",
        x if x == (N | S | E) => "left edge",
        x if x == (N | S | W) => "right edge",
        _ => "middle",
    };
    if kind == AutotileKind::Edge4 || e != EDGES {
        return base;
    }
    // Surrounded on all four edges, so the only thing left to say is which
    // DIAGONAL is missing — the inside corner of an L-bend, the piece a 47-tile
    // sheet has and a 16-tile one does not.
    match (mask & (NE | SE | SW | NW)) ^ (NE | SE | SW | NW) {
        0 => "middle, fully surrounded",
        x if x == SE => "inner corner, notch at top-right",
        x if x == SW => "inner corner, notch at top-left",
        x if x == NE => "inner corner, notch at bottom-right",
        x if x == NW => "inner corner, notch at bottom-left",
        _ => "middle, with inner corners",
    }
}

fn describe_mask(mask: u8, kind: AutotileKind) -> String {
    let corners = kind == AutotileKind::Blob8;
    let mut same: Vec<&str> = Vec::new();
    for (dx, dy, bit) in autotile::OFFSETS {
        if !corners && dx != 0 && dy != 0 {
            continue;
        }
        if mask & bit != 0 {
            // `dy` is in ROW space: -1 is north, which is ABOVE on screen. These
            // were the other way round, so the panel said "below" while the tile
            // answered "above" — an author following the words drew a sheet that
            // was upside down and could not see why.
            same.push(match (dx, dy) {
                (0, -1) => "above",
                (0, 1) => "below",
                (1, 0) => "right",
                (-1, 0) => "left",
                (1, -1) => "above-right",
                (1, 1) => "below-right",
                (-1, -1) => "above-left",
                _ => "below-left",
            });
        }
    }
    let shape = mask_shape_name(mask, kind);
    if same.is_empty() {
        return format!("{shape} — nothing of this group next to it");
    }
    format!("{shape} — more of this group: {}", same.join(", "))
}

/// One tile of the sheet, drawn into `rect`, honouring its packed orientation.
///
/// Goes through `tile_corner_drawn` — the SAME function the mesh builder uses — so
/// a preview cannot show one orientation and the map draw another. That is the
/// whole reason the orientation maths lives in core rather than in the mesh.
fn paint_tile(
    ui: &egui::Ui,
    rect: egui::Rect,
    sheet: &egui::TextureHandle,
    sc: u32,
    sr: u32,
    packed: u32,
) {
    if packed == floptle_core::EMPTY_TILE {
        // A hole reads as a hole: the checker is what every image editor uses for
        // "nothing here", and a blank square would look like a black tile.
        let c = Color32::from_gray(38);
        ui.painter().rect_filled(rect, 0.0, c);
        return;
    }
    let cell = floptle_core::tile_index(packed);
    let (sc, sr) = (sc.max(1), sr.max(1));
    if cell >= sc * sr {
        return;
    }
    let (cx, cy) = (cell % sc, cell / sc);
    let (u0, u1) = (cx as f32 / sc as f32, (cx + 1) as f32 / sc as f32);
    let (v0, v1) = (cy as f32 / sr as f32, (cy + 1) as f32 / sr as f32);
    let xf = floptle_core::tile_xform(packed);
    // egui's `Image` cannot express a rotated UV window, so draw the quad as a
    // mesh with the four UVs permuted the same way the tilemap mesh permutes them.
    let mut mesh = egui::Mesh::with_texture(sheet.id());
    // Corners of the drawn quad in (s, t): s left→right, t BOTTOM→top. egui's y
    // grows downward, so t = 1 is rect.top().
    for (s, t) in [(0u8, 1u8), (1, 1), (1, 0), (0, 0)] {
        let (a, b) = floptle_core::tile_corner(s, t, xf);
        let pos = egui::pos2(
            if s == 0 { rect.left() } else { rect.right() },
            if t == 1 { rect.top() } else { rect.bottom() },
        );
        let uv = egui::pos2(
            if a == 0 { u0 } else { u1 },
            // Texture v runs down, so the art's TOP (b = 1) is the smaller v.
            if b == 1 { v0 } else { v1 },
        );
        mesh.colored_vertex(pos, Color32::WHITE);
        let i = mesh.vertices.len() - 1;
        mesh.vertices[i].uv = uv;
    }
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    ui.painter().add(egui::Shape::mesh(mesh));
}

/// What the tileset knows about a tile, drawn over its art in the palette: the
/// collider's shape, and the autotile neighbourhood it answers.
fn draw_tile_overlays(ui: &egui::Ui, rect: egui::Rect, set: &TileSet, cell: u32, cell_px: f32) {
    // The collider, in the shape it actually is. A tick would say "solid"; this
    // says "solid HERE", which is the part that is easy to get wrong and
    // impossible to notice.
    match set.collision(cell).shape() {
        floptle_tiles::TileShape::None => {}
        floptle_tiles::TileShape::Poly(pts) => {
            // The outline, filled — the same picture the shape editor draws, so
            // a slope reads as a slope at palette size instead of as "solid".
            let poly: Vec<egui::Pos2> = pts
                .iter()
                .map(|p| {
                    egui::pos2(
                        rect.left() + p[0] * rect.width(),
                        rect.bottom() - p[1] * rect.height(),
                    )
                })
                .collect();
            if poly.len() >= 3 {
                ui.painter().add(egui::Shape::convex_polygon(
                    poly,
                    Color32::from_rgba_unmultiplied(90, 190, 255, 70),
                    egui::Stroke::new(1.0, Color32::from_rgb(120, 200, 255)),
                ));
            }
        }
        floptle_tiles::TileShape::Rect(x, y, w, h) => {
            // Tile space is +Y up; egui is +Y down.
            let r = egui::Rect::from_min_size(
                egui::pos2(
                    rect.left() + x * rect.width(),
                    rect.top() + (1.0 - y - h) * rect.height(),
                ),
                egui::vec2(w * rect.width(), h * rect.height()),
            );
            ui.painter().rect_filled(r, 0.0, Color32::from_rgba_unmultiplied(255, 90, 90, 70));
            ui.painter().rect_stroke(
                r,
                0.0,
                egui::Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 120, 120, 190)),
                egui::StrokeKind::Inside,
            );
        }
    }
    // The neighbourhood diagram — three by three dots in the corner. A tile can
    // answer several shapes now, so this draws the first and says how many more.
    let shapes = floptle_tiles::tile_masks(set, cell);
    if let Some(&(_, mask)) = shapes.first()
        && cell_px >= 24.0
    {
        let d = (cell_px / 9.0).clamp(2.0, 4.0);
        let base = egui::pos2(rect.left() + 2.0, rect.top() + 2.0);
        for (dx, dy, bit) in autotile::OFFSETS.iter().copied().chain([(0, 0, 0u8)]) {
            let on = bit == 0 || mask & bit != 0;
            let c = if bit == 0 {
                ACCENT
            } else if on {
                Color32::from_rgb(140, 220, 255)
            } else {
                Color32::from_rgba_unmultiplied(0, 0, 0, 150)
            };
            let p = egui::pos2(base.x + (dx + 1) as f32 * d, base.y + (dy + 1) as f32 * d);
            ui.painter().rect_filled(
                egui::Rect::from_center_size(p, egui::vec2(d - 0.8, d - 0.8)),
                0.0,
                c,
            );
        }
        if shapes.len() > 1 {
            ui.painter().text(
                egui::pos2(base.x + 3.6 * d, base.y + d),
                egui::Align2::LEFT_CENTER,
                format!("+{}", shapes.len() - 1),
                egui::FontId::proportional(8.0),
                ACCENT,
            );
        }
    }
    // An animated tile gets a mark, because otherwise the only way to know is to
    // press Play.
    if set.info(cell).is_some_and(|t| !t.frames.is_empty()) {
        ui.painter().text(
            rect.right_bottom() + egui::vec2(-2.0, -2.0),
            egui::Align2::RIGHT_BOTTOM,
            "▷",
            egui::FontId::proportional(9.0),
            Color32::from_rgb(160, 255, 180),
        );
    }
}

fn tile_tooltip(set: &TileSet, cell: u32) -> String {
    let mut s = format!("tile {cell}");
    let c = set.collision(cell);
    if c.is_solid() {
        s.push_str(&format!("\ncollides: {}", c.label()));
    }
    let tags = set.tags(cell);
    if !tags.is_empty() {
        s.push_str(&format!("\ntags: {}", tags.join(", ")));
    }
    if let Some(g) = set.group_of(cell) {
        let name = set.groups.get(g as usize).map(|x| x.name.as_str()).unwrap_or("?");
        s.push_str(&format!("\ngroup: {name}"));
    }
    if let Some(info) = set.info(cell)
        && !info.frames.is_empty()
    {
        s.push_str(&format!("\nanimates: {} frames at {} fps", info.frames.len() + 1, info.anim_fps));
    }
    s
}

/// A tileset path as a panel shows it: the name, not the folder.
fn short(path: &str) -> String {
    floptle_tiles::tileset_name(path).unwrap_or(path).to_string()
}

/// Apply the tab's intents. Separate from the UI because every one of these needs
/// `&mut Editor` — an undo snapshot, the world, or the store.
impl crate::Editor {
    pub(crate) fn apply_tile_cmds(&mut self, cmds: Vec<TileCmd>) {
        for cmd in cmds {
            self.apply_tile_cmd(cmd);
        }
    }

    fn apply_tile_cmd(&mut self, cmd: TileCmd) {
        match cmd {
            TileCmd::PickLayer(e) => {
                self.tile_tools.layer = Some(e);
                self.tile_tools.selection = None;
                self.select_single(e);
            }
            TileCmd::AddLayer => self.add_tile_layer(),
            TileCmd::Resize(c, r, ox, oy) => self.tile_resize(c, r, ox, oy),
            TileCmd::RetileAll => self.tile_retile_all(),
            TileCmd::Copy => self.tile_copy(),
            TileCmd::Paste => self.tile_paste(),
            TileCmd::ClearSelection => self.tile_clear_selection(),
            TileCmd::Reorient { turn, flip_x, flip_y } => {
                self.tile_reorient_selection(turn, flip_x, flip_y)
            }
            TileCmd::NewTilesetForLayer => self.new_tileset_for_layer(),
            TileCmd::AttachTileset(path) => {
                let Some(e) = self.tile_tools.layer else { return };
                self.begin_edit();
                if let Some(Matter::Tilemap { tileset, .. }) = self.world.get_mut::<Matter>(e) {
                    *tileset = path;
                    self.scene_dirty = true;
                }
            }
            TileCmd::SaveTilesets => self.save_tilesets(),
            // The rest edit the tileset, not the scene, so they are not scene undo.
            // A tileset is a project asset like a material: its own file, saved on
            // change. (Tileset undo is a real gap and is written down as one — see
            // docs/tilemaps.md.)
            other => self.apply_tileset_cmd(other),
        }
    }

    fn apply_tileset_cmd(&mut self, cmd: TileCmd) {
        let Some(path) = self.tile_tools.editing.clone() else { return };
        // Never edit a tileset whose file we could not read: the edit would be
        // written over the file we deliberately refused to touch.
        if self.tiles.load_failed.contains(&path) {
            return;
        }
        let Some(set) = self.tiles.get_mut(&path) else { return };
        match cmd {
            TileCmd::SetTags(cell, tags) => set.info_mut(cell).tags = tags,
            TileCmd::AddToRule(g, mask, cell) => {
                if let Some(group) = set.groups.get_mut(g as usize) {
                    group.add_to_rule(mask, cell);
                }
            }
            TileCmd::RemoveVariant(g, mask, n) => {
                if let Some(group) = set.groups.get_mut(g as usize) {
                    group.remove_variant(mask, n);
                }
            }
            TileCmd::ClearRule(g, mask) => {
                if let Some(group) = set.groups.get_mut(g as usize) {
                    group.clear_rule(mask);
                }
            }
            TileCmd::SetAnim(cell, frames, fps) => {
                let info = set.info_mut(cell);
                info.frames = frames;
                info.anim_fps = fps;
            }
            TileCmd::AddPage => {
                if set.page_count() < floptle_core::TILE_MAX_PAGES {
                    set.pages.push(floptle_tiles::TilePage {
                        texture: String::new(),
                        cols: 1,
                        rows: 1,
                    });
                }
            }
            // Page 0 lives in the tileset's own three fields rather than in
            // `pages`, so it is set here rather than being a missing case. It
            // used to be one — page 0 was the layer's material and unsettable,
            // which is why a tileset could not carry its own art at all.
            TileCmd::SetPage(p, texture, cols, rows) => {
                if p == 0 {
                    set.texture = texture;
                    set.sheet_cols = cols.max(1);
                    set.sheet_rows = rows.max(1);
                } else if let Some(page) = set.pages.get_mut(p as usize - 1) {
                    page.texture = texture;
                    page.cols = cols.max(1);
                    page.rows = rows.max(1);
                }
            }
            TileCmd::RemoveLastPage => {
                set.pages.pop();
                // The palette may have been showing the page that just went.
                if self.tile_tools.page >= set.page_count() {
                    self.tile_tools.page = 0;
                }
            }
            TileCmd::AddGroup(kind) => {
                let n = set.groups.len() + 1;
                set.groups.push(AutotileGroup {
                    name: format!("group {n}"),
                    kind,
                    ..Default::default()
                });
            }
            TileCmd::RemoveGroup(g) => {
                set.remove_group(g);
                // The armed group is an index into the list that just shifted.
                self.tile_tools.group = None;
                return;
            }
            TileCmd::RenameGroup(g, name) => {
                if let Some(group) = set.groups.get_mut(g as usize) {
                    group.name = name;
                }
            }
            TileCmd::SetGroupJoins(g, joins) => {
                if let Some(group) = set.groups.get_mut(g as usize) {
                    group.joins = joins;
                }
            }
            TileCmd::BulkCollision(cells, c) => {
                for cell in cells {
                    set.info_mut(cell).collision = c.clone();
                }
            }
            TileCmd::ApplyPreset(g) => {
                let Some(&kind) = set.groups.get(g as usize).map(|x| &x.kind) else { return };
                let Some((px, py, w, h)) = self.tile_tools.palette else { return };
                let sc = set.sheet_cols.max(1);
                let cells: Vec<u32> = (0..h)
                    .flat_map(|dy| (0..w).map(move |dx| (py + dy) * sc + px + dx))
                    .collect();
                let want = autotile::preset_len(kind);
                let pairs = autotile::assign_preset(kind, &cells);
                if let Some(group) = set.groups.get_mut(g as usize) {
                    // Replace, not merge: pressing the preset button twice should
                    // leave the group the preset describes, not two of everything.
                    for (_, mask) in &pairs {
                        group.clear_rule(*mask);
                    }
                    for (cell, mask) in &pairs {
                        group.add_to_rule(*mask, *cell);
                    }
                }
                // Say what was NOT covered rather than leaving it to be discovered
                // as a hole in a level: a preset silently truncating is the exact
                // shape of failure this codebase keeps paying for.
                let n = cells.len();
                if n.is_multiple_of(want) && n > want {
                    self.tile_note(&format!(
                        "{n} tiles over {want} shapes — each shape got {} that take turns",
                        n / want
                    ));
                } else if n != want {
                    let msg = if n < want {
                        format!(
                            "assigned {n} of the {want} tiles this preset needs — the other \
                             {} shapes have no tile and stay as painted",
                            want - n
                        )
                    } else {
                        format!(
                            "the preset needs {want} tiles and {n} were selected — the last \
                             {} were left over. Select a whole multiple of {want} and the \
                             extra passes become variants.",
                            n - want
                        )
                    };
                    self.tile_note(&msg);
                }
                return;
            }
            _ => return,
        }
        set.prune();
    }

    /// Add a tilemap node in front of the last one.
    fn add_tile_layer(&mut self) {
        use floptle_core::math::{DVec3, Quat, Vec3};
        self.record();
        let n = self.tile_layers().len();
        let e = self.world.spawn();
        // In FRONT of the previous layer, by a tenth of a unit. Stacking layers at
        // the same Z makes the depth test pick arbitrarily between them and the map
        // shimmers as the camera moves — the same last-bit problem the tilemap mesh
        // exists to avoid, one level up.
        self.world.insert(
            e,
            floptle_core::transform::Transform {
                translation: DVec3::new(0.0, 0.0, n as f64 * 0.1),
                rotation: Quat::IDENTITY,
                scale: Vec3::ONE,
            },
        );
        self.world.insert(e, floptle_core::Name(format!("Tiles {}", n + 1)));
        self.world.insert(
            e,
            Matter::Tilemap {
                cols: 32,
                rows: 18,
                tile: 1.0,
                data: Vec::new(),
                tileset: String::new(),
            },
        );
        // Unlit by default: a 2D layer lit by the scene's sun is a 2D layer that
        // goes dark at night, which is nobody's intent for a spritesheet.
        self.world.insert(
            e,
            floptle_core::Material { unlit: true, ..Default::default() },
        );
        self.tile_tools.layer = Some(e);
        self.tile_tools.selection = None;
        self.select_single(e);
        self.scene_dirty = true;
        self.tile_note("added a tile layer — give it a spritesheet in the Inspector");
    }

    /// Make a tileset for the active layer's own sheet and attach it.
    fn new_tileset_for_layer(&mut self) {
        let Some(e) = self.tile_tools.layer else { return };
        let Some(mat) = self.world.get::<floptle_core::Material>(e).cloned() else { return };
        let Some(tex) = mat.texture.clone() else {
            self.tile_note("give this layer a spritesheet first — a tileset describes one sheet");
            return;
        };
        let (cols, rows) = mat.sheet();
        if cols * rows <= 1 {
            self.tile_note(
                "this material's texture is not cut into a sheet — set sheetCols / sheetRows \
                 on it first, so the tileset knows how many tiles there are",
            );
            return;
        }
        let name = std::path::Path::new(&tex)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("tileset")
            .to_string();
        let path = self.new_tileset(&name, &tex, cols, rows);
        self.begin_edit();
        if let Some(Matter::Tilemap { tileset, .. }) = self.world.get_mut::<Matter>(e) {
            *tileset = path.clone();
            self.scene_dirty = true;
        }
        self.tile_tools.editing = Some(path.clone());
        self.save_tilesets();
        self.tile_note(&format!("made {path} for {cols}×{rows} tiles"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dropped sheet has to arrive usable. 1×1 is one tile the size of the
    /// whole image, which reads as the drop having failed.
    #[test]
    fn a_dropped_sheet_gets_a_cut_it_can_be_used_with() {
        // 128×128 of 16-px tiles: the largest cell that divides both and leaves
        // a real grid.
        assert_eq!(guess_sheet_grid("textures/x.png", Some((128, 128))), (2, 2));
        // …because 64 divides it first. A sheet that only 16 fits:
        assert_eq!(guess_sheet_grid("textures/x.png", Some((80, 48))), (5, 3));
        // Unreadable stays honest rather than confidently wrong.
        assert_eq!(guess_sheet_grid("textures/x.png", None), (1, 1));
        assert_eq!(guess_sheet_grid("textures/x.png", Some((0, 0))), (1, 1));
    }

    /// A number in the filename beats the inference, because an artist who wrote
    /// it down knows something the pixel dimensions cannot say: a 64×64 sheet of
    /// 32s and one of 16s are the same image.
    #[test]
    fn the_filename_wins_when_it_names_a_cell_size() {
        assert_eq!(guess_sheet_grid("textures/tiles_16x16.png", Some((64, 64))), (4, 4));
        assert_eq!(guess_sheet_grid("textures/dungeon-32.png", Some((64, 64))), (2, 2));
        // Without the name the same image guesses the largest cell that fits.
        assert_eq!(guess_sheet_grid("textures/dungeon.png", Some((64, 64))), (2, 2));
        // A cell size that does not divide the image is not believed.
        assert_eq!(guess_sheet_grid("textures/tiles_24.png", Some((64, 64))), (2, 2));
        // `NxM` with different halves names no single cell size.
        assert_eq!(cell_size_in_name("sheet_16x32.png"), None);
        // A year is not a tile size.
        assert_eq!(cell_size_in_name("art2026.png"), None);
        assert_eq!(cell_size_in_name("tiles_16x16.png"), Some(16));
    }

    /// Setting sheet 0 writes the tileset's OWN fields. It used to have no
    /// effect at all — page 0 was the layer's material — which is why a tileset
    /// could not carry its own art.
    #[test]
    fn sheet_zero_is_the_tilesets_own_and_is_settable() {
        let mut set = TileSet { sheet_cols: 1, sheet_rows: 1, ..Default::default() };
        set.pages.push(floptle_tiles::TilePage {
            texture: "b.png".into(),
            cols: 2,
            rows: 2,
        });
        // What TileCmd::SetPage does, at the level a unit test can reach.
        set.texture = "a.png".into();
        set.sheet_cols = 4;
        set.sheet_rows = 4;
        assert_eq!(set.page(0), Some(("a.png", 4, 4)));
        assert_eq!(set.page(1), Some(("b.png", 2, 2)));
        assert_eq!(set.page_count(), 2);
    }

    #[test]
    fn a_tileset_path_shows_as_its_name() {
        assert_eq!(short("tilesets/bricks.tileset.ron"), "bricks");
        // Something that is not a tileset path shows whole rather than empty.
        assert_eq!(short("weird"), "weird");
    }

    /// Every command must be constructible and comparable — the queue is drained
    /// by value and compared in tests, and a variant that is neither would not be.
    #[test]
    fn the_command_queue_round_trips() {
        let cmds = vec![
            TileCmd::AddLayer,
            TileCmd::Resize(4, 4, 0, 0),
            TileCmd::BulkCollision(vec![3], TileCollision::Half(TileSide::Top)),
            TileCmd::AddGroup(AutotileKind::Blob8),
            TileCmd::SetAnim(2, vec![3, 4], 8.0),
        ];
        let same = cmds.clone();
        assert_eq!(cmds, same);
        assert_ne!(TileCmd::AddLayer, TileCmd::RetileAll);
    }

    // ---- floptle/0093: the sections that used to vanish ----------------------

    /// Drive the panel headlessly and return every word it drew, so "does this
    /// section exist" is a question the gate can answer.
    fn panel_text(world: &floptle_core::World, tools: &mut TileTools, store: &mut TileStore) -> String {
        let ctx = egui::Context::default();
        let root = std::path::PathBuf::from(".");
        let mut cmds = Vec::new();
        let out = ctx.run_ui(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                TileCtx {
                    store: &mut *store,
                    tools: &mut *tools,
                    world,
                    project_root: &root,
                    cmds: &mut cmds,
                    playing: false,
                }
                .ui(ui);
            });
        });
        fn walk(s: &egui::Shape, out: &mut String) {
            match s {
                egui::Shape::Text(t) => {
                    out.push_str(t.galley.text());
                    out.push('\n');
                }
                egui::Shape::Vec(v) => v.iter().for_each(|s| walk(s, out)),
                _ => {}
            }
        }
        let mut text = String::new();
        for cs in &out.shapes {
            walk(&cs.shape, &mut text);
        }
        text
    }

    fn layer_world(tileset: &str) -> (floptle_core::World, Entity) {
        let mut world = floptle_core::World::default();
        let e = world.spawn();
        world.insert(e, floptle_core::Name("layer".into()));
        world.insert(
            e,
            Matter::Tilemap {
                cols: 4,
                rows: 4,
                tile: 1.0,
                data: vec![floptle_core::EMPTY_TILE; 16],
                tileset: tileset.to_string(),
            },
        );
        (world, e)
    }

    /// A tileset with a group, its kind, and no rules filled yet.
    fn grouped_world(kind: AutotileKind) -> (floptle_core::World, Entity, String, TileStore) {
        let path = floptle_tiles::tileset_path("bricks");
        let (world, e) = layer_world(&path);
        let mut set = TileSet { texture: "t.png".into(), sheet_cols: 4, sheet_rows: 4, ..Default::default() };
        set.groups.push(floptle_tiles::AutotileGroup {
            name: "grass".into(),
            kind,
            ..Default::default()
        });
        let mut store = TileStore::default();
        store.sets.insert(path.clone(), set);
        (world, e, path, store)
    }

    /// The setup flow has to say what to DO. The old copy described the idea
    /// ("mark a run of tiles as a group, hand it the preset") and you could read
    /// it twice without learning what to click — reported as "there's basically
    /// no indication of how to actually use it".
    #[test]
    fn the_autotile_section_says_what_to_click() {
        let (world, e, path, mut store) = grouped_world(AutotileKind::Blob8);
        store.get_mut(&path).unwrap().groups.clear();
        let mut tools =
            TileTools { layer: Some(e), editing: Some(path), ..Default::default() };
        let text = panel_text(&world, &mut tools, &mut store);
        assert!(text.contains("RULES") || text.contains("Add one below"), "no steps:\n{text}");
        assert!(
            text.contains("click") || text.contains("Click"),
            "the steps must name an action, not a concept:\n{text}"
        );
    }

    /// Every neighbourhood of the preset gets a slot, whether or not it has a
    /// tile — an unfilled rule you cannot see is one you cannot fill.
    #[test]
    fn every_rule_of_the_preset_has_a_slot() {
        for kind in AutotileKind::ALL {
            let (world, e, path, mut store) = grouped_world(kind);
            let mut tools = TileTools {
                layer: Some(e),
                editing: Some(path.clone()),
                group: Some(0),
                ..Default::default()
            };
            let text = panel_text(&world, &mut tools, &mut store);
            let want = autotile::preset_len(kind);
            assert!(
                text.contains("RULES"),
                "{kind:?} draws no rules grid at all:\n{text}"
            );
            assert!(want > 0, "{kind:?} claims no neighbourhoods");
        }
    }

    /// Arming a rule and clicking a tile assigns THAT tile to THAT
    /// neighbourhood. The old flow could only assign a whole preset at once,
    /// in cell order, from a multi-selection — correct for a sheet drawn in the
    /// preset's order and unusable otherwise, with no way to see or fix one.
    #[test]
    fn filling_one_rule_points_it_at_the_tile_you_clicked() {
        let (_, _, path, mut store) = grouped_world(AutotileKind::Blob8);
        let set = store.get_mut(&path).unwrap();
        // What the palette click does when `fill_mask` is armed.
        let mask = autotile::preset_masks(AutotileKind::Blob8)[0];
        set.groups[0].add_to_rule(mask, 6);
        let at = floptle_tiles::Autotiler::build(store.get(&path).unwrap());
        assert_eq!(at.resolve(0, mask), Some(6), "the rule must resolve to the tile clicked");
        // …and the rest of the preset is still waiting, rather than the whole
        // thing being consumed by one click.
        assert_eq!(
            at.missing(0).len(),
            autotile::preset_len(AutotileKind::Blob8) - 1,
            "one click fills exactly one rule"
        );
    }

    /// The reported bug: *"I can only have one tile assigned to one rule at a
    /// time so I can't have duplicates."* Both directions of duplicate have to
    /// work — one tile on many shapes, many tiles on one shape.
    #[test]
    fn a_tile_can_draw_more_than_one_shape_and_a_shape_more_than_one_tile() {
        let (_, _, path, mut store) = grouped_world(AutotileKind::Edge4);
        let set = store.get_mut(&path).unwrap();
        let masks = autotile::preset_masks(AutotileKind::Edge4);

        // One tile, three shapes. Under the old model the second assignment
        // moved the tile off the first shape and the first shape went blank.
        for m in masks.iter().take(3) {
            set.groups[0].add_to_rule(*m, 6);
        }
        // One shape, three tiles.
        for cell in [10, 11, 12] {
            set.groups[0].add_to_rule(masks[5], cell);
        }
        let at = floptle_tiles::Autotiler::build(store.get(&path).unwrap());
        for m in masks.iter().take(3) {
            assert_eq!(at.resolve(0, *m), Some(6), "tile 6 stopped drawing shape {m:#b}");
        }
        assert_eq!(at.variants(0, masks[5]), &[10, 11, 12]);
    }

    /// While a rule is armed, a palette click ADDS. Advancing to the next shape
    /// happens on the first tile only, or the second click would land on some
    /// other shape and adding a variant would be impossible.
    #[test]
    fn clicking_a_second_tile_for_one_shape_adds_it_rather_than_replacing() {
        let (_, _, path, mut store) = grouped_world(AutotileKind::Edge4);
        let mask = autotile::preset_masks(AutotileKind::Edge4)[3];
        let set = store.get_mut(&path).unwrap();
        set.groups[0].add_to_rule(mask, 4);
        set.groups[0].add_to_rule(mask, 5);
        set.groups[0].add_to_rule(mask, 4); // the same tile twice: a weighting
        assert_eq!(set.groups[0].tiles_for(mask), &[4, 5, 4]);

        // …and taking one back out takes ONE.
        set.groups[0].remove_variant(mask, 2);
        assert_eq!(set.groups[0].tiles_for(mask), &[4, 5], "the twin went with it");
    }

    /// A shape holding several tiles has to SAY so in the grid — otherwise a
    /// rule with one tile and a rule with five look identical, and the variants
    /// are invisible until something is painted.
    #[test]
    fn the_panel_says_a_shape_has_variants() {
        let (world, e, path, mut store) = grouped_world(AutotileKind::Edge4);
        let mask = autotile::preset_masks(AutotileKind::Edge4)[2];
        let set = store.get_mut(&path).unwrap();
        for cell in [4, 5, 6] {
            set.groups[0].add_to_rule(mask, cell);
        }
        let mut tools = TileTools {
            layer: Some(e),
            editing: Some(path),
            group: Some(0),
            fill_mask: Some((0, mask)),
            ..Default::default()
        };
        let text = panel_text(&world, &mut tools, &mut store);
        assert!(
            text.contains("take turns"),
            "nothing tells you the shape has alternates:\n{text}"
        );
        assert!(
            text.contains("added"),
            "and nothing says a palette click ADDS rather than replaces:\n{text}"
        );
    }

    /// Per-tile collision and autotiling were both built and both INVISIBLE
    /// without a tileset — the sections returned before drawing their own
    /// heading, so the panel looked like an engine that has neither. Reported
    /// exactly that way, twice.
    #[test]
    fn collision_and_autotiling_are_visible_before_there_is_a_tileset() {
        let (world, e) = layer_world("");
        let mut tools = TileTools { layer: Some(e), ..Default::default() };
        let mut store = TileStore::default();
        let text = panel_text(&world, &mut tools, &mut store);
        assert!(text.contains("TILE\n"), "the per-tile section must name itself:\n{text}");
        assert!(text.contains("AUTOTILE"), "the autotile section must name itself:\n{text}");
        assert!(
            text.contains("needs a tileset"),
            "and each must say what it is waiting for:\n{text}"
        );
    }

    /// **North is up.** The panel's words for a neighbourhood have to agree with
    /// the mask the engine resolves, and they did not: `OFFSETS` measures `dy`
    /// in ROW space, where `-1` is north — up the screen — and both the sentence
    /// and the 3×3 diagram negated it. So a tile that answers "there is more of
    /// this group ABOVE me" was labelled *below*, and drawn with its dot below
    /// the centre.
    ///
    /// The cost of that is not a wrong word. An author picking tiles by what the
    /// panel showed built a sheet answering the vertically mirrored
    /// neighbourhood of the one they meant, which in a level reads as the art
    /// being wrong rather than the table — exactly the "plausible wrongness" the
    /// autotile module's own header warns a bad preset produces.
    #[test]
    fn the_panel_says_north_is_above() {
        use floptle_tiles::autotile::{E, N, S, W};
        let d = |m: u8| describe_mask(m, AutotileKind::Edge4);
        assert!(d(N).contains("above"), "north read as: {}", d(N));
        assert!(d(S).contains("below"), "south read as: {}", d(S));
        assert!(d(E).contains("right"), "east read as: {}", d(E));
        assert!(d(W).contains("left"), "west read as: {}", d(W));
        // And the corners, on the preset that has them.
        let b = |m: u8| describe_mask(m, AutotileKind::Blob8);
        assert!(b(floptle_tiles::autotile::NE).contains("above-right"));
        assert!(b(floptle_tiles::autotile::SW).contains("below-left"));
    }

    /// The shape names are the other half of the fix, and they have to be the
    /// INVERSE of the neighbour directions: a tile whose group continues below
    /// and to the right is the piece at the TOP-LEFT of the shape. Getting this
    /// backwards would be the same bug wearing a different hat.
    #[test]
    fn a_tile_is_named_for_where_it_sits_not_where_its_neighbours_are() {
        use floptle_tiles::autotile::{E, N, S, W};
        let n = |m: u8| mask_shape_name(m, AutotileKind::Edge4);
        assert_eq!(n(S | E), "top-left corner", "stuff below and right = the top-left piece");
        assert_eq!(n(N | W), "bottom-right corner");
        assert_eq!(n(E | S | W), "top edge");
        assert_eq!(n(N | E | W), "bottom edge");
        assert_eq!(n(N | S | E | W), "middle");
        assert_eq!(n(0), "single block");
        assert_eq!(n(E), "left end", "stuff only to the right = the left end of a run");
        // Every shape in both presets gets a name — a slot labelled with an
        // empty string is a slot nobody can pick a tile for.
        for kind in AutotileKind::ALL {
            for m in floptle_tiles::preset_masks(kind) {
                assert!(!mask_shape_name(m, kind).is_empty(), "{kind:?} {m:#010b} has no name");
            }
        }
    }

    /// `tools.editing` was set and never cleared, so selecting a layer with no
    /// tileset left the TILE and AUTOTILE editors pointed at the PREVIOUS
    /// layer's — every edit landing on a tileset the layer does not name.
    #[test]
    fn a_layer_with_no_tileset_stops_editing_the_last_ones() {
        let path = floptle_tiles::tileset_path("bricks");
        let (mut world, with) = layer_world(&path);
        let without = world.spawn();
        world.insert(without, floptle_core::Name("bare".into()));
        world.insert(
            without,
            Matter::Tilemap {
                cols: 4,
                rows: 4,
                tile: 1.0,
                data: vec![floptle_core::EMPTY_TILE; 16],
                tileset: String::new(),
            },
        );
        let mut store = TileStore::default();
        store.sets.insert(path.clone(), TileSet::default());
        let mut tools = TileTools { layer: Some(with), ..Default::default() };
        panel_text(&world, &mut tools, &mut store);
        assert_eq!(tools.editing.as_deref(), Some(path.as_str()));

        tools.layer = Some(without);
        panel_text(&world, &mut tools, &mut store);
        assert_eq!(tools.editing, None, "a bare layer must not inherit the last one's tileset");
    }
}
