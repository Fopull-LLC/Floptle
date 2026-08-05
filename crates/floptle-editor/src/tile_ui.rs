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
    AutotileGroup, AutotileKind, Stamp, TileCollision, TileSet, TileSide, autotile, tile_mask,
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
    /// Set one tile's collision.
    SetCollision(u32, TileCollision),
    /// Replace one tile's tag list.
    SetTags(u32, Vec<String>),
    /// Put a tile in a group (`None` = take it out).
    SetGroup(u32, Option<u16>),
    /// Set a tile's neighbour mask.
    SetMask(u32, u8),
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
        // A tileset whose sheet grid disagrees with the layer's material is the
        // quiet failure this catches: every cell index would mean a different
        // picture, so the map would draw scrambled art with nothing said.
        let (msc, msr) = self.sheet_size();
        if (msc, msr) != (sc, sr) {
            ui.colored_label(
                ACCENT,
                format!("⚠ this layer's material is cut {msc}×{msr}, the tileset says {sc}×{sr}"),
            );
            ui.small(
                "Every cell index means a different tile under the two, so the map draws \
                 the wrong art. Fix whichever is wrong — neither is guessed at.",
            );
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
        for (p, tex, c, r) in set.pages_iter().collect::<Vec<_>>() {
            if p == 0 {
                ui.small(format!("0 — this layer's material, {c}×{r}"));
                continue;
            }
            let (mut path, mut cols, mut rows) = (tex.to_string(), c, r);
            let mut changed = false;
            ui.horizontal(|ui| {
                ui.small(format!("{p}"));
                changed |= ui
                    .add(egui::TextEdit::singleline(&mut path).desired_width(120.0).hint_text("textures/…png"))
                    .on_hover_text("project-relative image for this sheet")
                    .lost_focus();
                changed |= ui.add(egui::DragValue::new(&mut cols).range(1..=256).prefix("c")).changed();
                changed |= ui.add(egui::DragValue::new(&mut rows).range(1..=256).prefix("r")).changed();
            });
            if changed {
                self.cmds.push(TileCmd::SetPage(p, path, cols, rows));
            }
        }
    }

    // ---- PALETTE ------------------------------------------------------------

    fn palette_section(&mut self, ui: &mut egui::Ui) {
        section(ui, "PALETTE");
        let set = self.tools.editing.clone().and_then(|p| self.store.get(&p)).cloned();
        // Which sheet of the tileset we are picking from. A tileset with pages
        // draws a row of tabs; without one there is a single implicit page and
        // nothing extra on screen (`floptle/0092`).
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
        // Page 0 is the LAYER'S material, unchanged; later pages are the
        // tileset's own images.
        let (sc, sr) = match (page, set.as_ref()) {
            (0, _) => self.sheet_size(),
            (_, Some(s)) => s.page(page).map(|(_, c, r)| (c, r)).unwrap_or((1, 1)),
            (_, None) => (1, 1),
        };
        let handle = if page == 0 {
            self.sheet_handle(ui)
        } else {
            set.as_ref()
                .and_then(|s| s.page(page))
                .filter(|(t, ..)| !t.is_empty())
                .and_then(|(t, ..)| self.texture_handle(ui, t))
        };
        let Some(sheet) = handle else {
            if page == 0 {
                ui.small(
                    "This layer's material has no texture yet — give it a spritesheet in the \
                     Inspector and set its sheetCols / sheetRows.",
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
        ui.small("Click a tile; drag for a multi-tile brush. Shift-click to inspect without arming.");
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

                        let in_sel = self
                            .tools
                            .palette
                            .is_some_and(|(px, py, w, h)| {
                                c >= px && c < px + w && r >= py && r < py + h
                            });
                        let ring = if in_sel {
                            ACCENT
                        } else if resp.hovered() {
                            Color32::from_gray(210)
                        } else {
                            Color32::from_gray(70)
                        };
                        ui.painter().rect_stroke(
                            rect,
                            0.0,
                            egui::Stroke::new(if in_sel { 2.0 } else { 1.0 }, ring),
                            egui::StrokeKind::Inside,
                        );

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
            self.tools.inspect_cell = Some(floptle_core::tile_cell_of(page, rect.1 * sc + rect.0));
            // A multi-square brush is a brush, not a group paint — a group resolves
            // per square and cannot honour a layout.
            self.tools.group = None;
        } else if let Some(idx) = clicked {
            let shift = ui.input(|i| i.modifiers.shift);
            self.tools.inspect_cell = Some(idx);
            if !shift {
                let local = floptle_core::tile_in_page(idx);
                self.tools.palette = Some((local % sc, local / sc, 1, 1));
                self.tools.stamp = Stamp::one(idx);
                // Clicking a tile that belongs to a group arms the GROUP: that is
                // what somebody clicking an autotile tile means, and arming the
                // literal tile would paint one fixed corner piece everywhere.
                self.tools.group =
                    set.as_ref().and_then(|s| s.group_of(idx)).filter(|_| self.tools.auto_retile);
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
        let Some(cell) = self.tools.inspect_cell else {
            ui.small("Click a tile in the palette above to set what it is.");
            return;
        };
        if cell >= set.cells() {
            return;
        }
        ui.label(RichText::new(format!("tile {cell}")).strong());

        // Collision. Four chips and, for Custom, four numbers.
        let coll = set.collision(cell);
        labelled(ui, "collides", |ui| {
            for (label, want) in [
                ("none", TileCollision::None),
                ("full", TileCollision::Full),
            ] {
                if ui
                    .add_sized([CHIP_W * 0.7, BTN_H], egui::Button::new(label).selected(coll == want))
                    .clicked()
                {
                    self.cmds.push(TileCmd::SetCollision(cell, want));
                }
            }
            let is_half = matches!(coll, TileCollision::Half(_));
            if ui
                .add_sized([CHIP_W * 0.7, BTN_H], egui::Button::new("half").selected(is_half))
                .clicked()
                && !is_half
            {
                self.cmds
                    .push(TileCmd::SetCollision(cell, TileCollision::Half(TileSide::Bottom)));
            }
            let is_rect = matches!(coll, TileCollision::Custom { .. });
            if ui
                .add_sized([CHIP_W * 0.7, BTN_H], egui::Button::new("rect").selected(is_rect))
                .clicked()
                && !is_rect
            {
                self.cmds.push(TileCmd::SetCollision(
                    cell,
                    TileCollision::Custom { x: 0.0, y: 0.0, w: 1.0, h: 0.5 },
                ));
            }
        });
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
                        self.cmds.push(TileCmd::SetCollision(cell, TileCollision::Half(s)));
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
                self.cmds.push(TileCmd::SetCollision(cell, TileCollision::Custom { x, y, w, h }));
            }
        }

        // The bulk path — the thing that makes a 256-tile sheet's collision a
        // minute's work rather than an afternoon's.
        if let Some((px, py, w, h)) = self.tools.palette
            && w * h > 1
        {
            let cells: Vec<u32> = (0..h)
                .flat_map(|dy| (0..w).map(move |dx| (py + dy, px + dx)))
                .map(|(r, c)| r * set.sheet_cols.max(1) + c)
                .collect();
            labelled(ui, "selection", |ui| {
                if ui
                    .add_sized([CHIP_W, BTN_H], egui::Button::new("All solid"))
                    .on_hover_text(format!("mark all {} selected tiles solid", cells.len()))
                    .clicked()
                {
                    self.cmds.push(TileCmd::BulkCollision(cells.clone(), TileCollision::Full));
                }
                if ui
                    .add_sized([CHIP_W, BTN_H], egui::Button::new("None solid"))
                    .clicked()
                {
                    self.cmds.push(TileCmd::BulkCollision(cells.clone(), TileCollision::None));
                }
            });
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
        ui.small(
            "A group of tiles that pick themselves by what is next to them. Mark a run of \
             tiles as a group, hand it the preset, and painting draws corners and edges.",
        );
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

                // Per-tile mask editing, for the tiles a preset got wrong.
                if let Some(cell) = self.tools.inspect_cell
                    && set.group_of(cell) == Some(g)
                    && let Some((_, mask)) = tile_mask(&set, cell)
                {
                    ui.separator();
                    ui.small(format!("tile {cell}'s neighbourhood"));
                    if let Some(next) = mask_editor(ui, mask, group.kind) {
                        self.cmds.push(TileCmd::SetMask(cell, next));
                    }
                }
                if let Some(cell) = self.tools.inspect_cell
                    && set.group_of(cell) != Some(g)
                    && ui.small_button(format!("put tile {cell} in this group")).clicked()
                {
                    self.cmds.push(TileCmd::SetGroup(cell, Some(g)));
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
    if let Some((x, y, w, h)) = set.collision(cell).rect() {
        // Tile space is +Y up; egui is +Y down.
        let r = egui::Rect::from_min_size(
            egui::pos2(rect.left() + x * rect.width(), rect.top() + (1.0 - y - h) * rect.height()),
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
    // The neighbourhood diagram — three by three dots in the corner.
    if let Some((_, mask)) = tile_mask(set, cell)
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

/// A clickable 3×3 neighbourhood: which neighbours this tile is the answer for.
/// Returns the new mask when a cell was clicked.
///
/// The corners are disabled for an `Edge4` group, because that kind does not
/// distinguish them — an editable control that changed nothing would be a lie
/// about what the rules are.
fn mask_editor(ui: &mut egui::Ui, mask: u8, kind: AutotileKind) -> Option<u8> {
    let mut out = None;
    let corners_live = kind == AutotileKind::Blob8;
    ui.vertical(|ui| {
        for dy in -1i32..=1 {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 1.0;
                for dx in -1i32..=1 {
                    if (dx, dy) == (0, 0) {
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
                        ui.painter().rect_filled(rect, 2.0, ACCENT);
                        continue;
                    }
                    let bit = autotile::OFFSETS
                        .iter()
                        .find(|(ox, oy, _)| *ox == dx && *oy == dy)
                        .map(|(_, _, b)| *b)
                        .unwrap_or(0);
                    let is_corner = dx != 0 && dy != 0;
                    let on = mask & bit != 0;
                    let enabled = corners_live || !is_corner;
                    let (rect, resp) = ui.allocate_exact_size(
                        egui::vec2(20.0, 20.0),
                        if enabled { egui::Sense::click() } else { egui::Sense::hover() },
                    );
                    let c = match (enabled, on) {
                        (false, _) => Color32::from_gray(45),
                        (true, true) => Color32::from_rgb(140, 220, 255),
                        (true, false) => Color32::from_gray(70),
                    };
                    ui.painter().rect_filled(rect, 2.0, c);
                    if enabled && resp.clicked() {
                        out = Some(mask ^ bit);
                    }
                    if !enabled {
                        resp.on_hover_text("an Edges group does not distinguish corners");
                    }
                }
            });
        }
    });
    ui.small("filled = a neighbour of this group. North is up.");
    out
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
            TileCmd::SetCollision(cell, c) => set.info_mut(cell).collision = c,
            TileCmd::SetTags(cell, tags) => set.info_mut(cell).tags = tags,
            TileCmd::SetGroup(cell, g) => {
                let info = set.info_mut(cell);
                info.group = g;
                if g.is_none() {
                    info.mask = 0;
                }
            }
            TileCmd::SetMask(cell, m) => set.info_mut(cell).mask = m,
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
            TileCmd::SetPage(p, texture, cols, rows) => {
                if p > 0
                    && let Some(page) = set.pages.get_mut(p as usize - 1)
                {
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
                set.groups.push(AutotileGroup { name: format!("group {n}"), kind, joins: vec![] });
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
                    set.info_mut(cell).collision = c;
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
                for cell in &cells {
                    let info = set.info_mut(*cell);
                    info.group = Some(g);
                }
                for (cell, mask) in autotile::assign_preset(kind, &cells) {
                    set.info_mut(cell).mask = mask;
                }
                // Say what was NOT covered rather than leaving it to be discovered
                // as a hole in a level: a preset silently truncating is the exact
                // shape of failure this codebase keeps paying for.
                let n = cells.len();
                if n != want {
                    let msg = if n < want {
                        format!(
                            "assigned {n} of the {want} tiles this preset needs — the other \
                             {} neighbourhoods have no tile and stay as painted",
                            want - n
                        )
                    } else {
                        format!(
                            "the preset needs {want} tiles and {n} were selected — the last \
                             {} got no mask (two tiles cannot answer one neighbourhood)",
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
            TileCmd::SetCollision(3, TileCollision::Half(TileSide::Top)),
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
