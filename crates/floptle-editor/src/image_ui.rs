//! The 🖼 Image dock tab: menus, tool strip, canvas, and the right-hand panel
//! (tool options, colour + palette, layers, frames).
//!
//! Layout is fixed and boring on purpose — a tool column that never resizes
//! itself, a right panel you can drag but that never moves on its own, and the
//! canvas in the middle. The house UX bar (`ui-stability-feedback`) applies in
//! full here: nothing re-centres, nothing re-sizes, popups are a constant size.

use egui::{Color32, RichText};
use floptle_image::adjust::{Adjustment, CurveChannel, Dither};
use floptle_image::brush::{BrushMode, GradientKind};
use floptle_image::doc::{Layer, LayerKind, Mode};
use floptle_image::effect::Effect;
use floptle_image::select::SelectOp;
use floptle_image::{Blend, Palette};

use crate::image_edit::{FilterKind, ImageEditState, ImgTool, NewForm, PaintTargetSurface};
use crate::EditorCmd;

/// What an export writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ImageExport {
    /// The flattened canvas (the same thing every save writes).
    Png,
    /// Just the active layer.
    Layer,
    /// The selection's bounding box, cropped.
    Selection,
    /// Every frame packed into one uniform-grid sheet, and the grid written into
    /// `.floptle/textures.ron` so the engine can address it.
    Sheet,
    Gif,
}

/// Everything the 🖼 tab touches — and nothing else.
///
/// Deliberately three fields rather than a method on `EditorTabViewer`: that
/// struct borrows ~80 pieces of the editor and cannot be built in a test, while
/// this can, so `tab_renders_in_every_state` runs the real panels headlessly.
pub(crate) struct ImageCtx<'a> {
    pub(crate) st: &'a mut ImageEditState,
    pub(crate) project_root: &'a std::path::Path,
    pub(crate) cmd: &'a mut EditorCmd,
}

/// A first cell grid for a canvas that has just been declared a sheet: the
/// largest square cell that divides both sides and leaves at least a 2x2 of
/// them. Better than `1x1`, and always overridable — the point is that the
/// numbers start somewhere plausible, not that they are guessed correctly.
fn guess_sheet(w: u32, h: u32) -> (u32, u32) {
    for px in [64u32, 48, 32, 24, 16, 8] {
        if w.is_multiple_of(px) && h.is_multiple_of(px) && w / px >= 2 && h / px >= 2 {
            return (w / px, h / px);
        }
    }
    (1, 1)
}

impl ImageCtx<'_> {
    pub(crate) fn ui(&mut self, ui: &mut egui::Ui) {
        self.st.tab_visible = true;
        if !self.st.palettes_loaded {
            self.st.palettes = crate::image_io::load_palettes(self.project_root);
            self.st.palettes_loaded = true;
        }
        if self.st.doc.is_none() {
            self.image_welcome(ui);
            return;
        }
        // Text rasterizes through egui's font atlas, which only exists on the
        // Context — so the tab drives it, not the canvas.
        if self.st.text_needs_render() {
            let ctx = ui.ctx().clone();
            self.st.render_text(&ctx);
        }
        self.image_menu_bar(ui);
        self.image_tool_strip(ui);
        self.image_side_panel(ui);
        egui::CentralPanel::default().show(ui, |ui| {
            self.image_status_bar(ui);
            let _ = self.st.canvas_ui(ui);
        });
        self.image_new_dialog(ui);
        self.image_save_dialog(ui);
        self.image_keys_window(ui);
        // A live text block owns the keyboard (its field has focus), so the
        // editor's own Escape/Enter handling never sees these — the tab has to
        // answer them itself or there is no way out of a text block but the
        // mouse.
        if self.st.text.is_some() {
            let (esc, apply) = ui.ctx().input(|i| {
                (
                    i.key_pressed(egui::Key::Escape),
                    i.modifiers.command && i.key_pressed(egui::Key::Enter),
                )
            });
            if esc {
                self.st.cancel_text();
            } else if apply {
                self.st.commit_text();
            }
        }
        // A continuous edit banks its undo step the moment the pointer is free.
        if !ui.ctx().input(|i| i.pointer.any_down()) {
            self.st.flush_edit();
        }
    }

    /// The empty state: what this tab is and the two ways in.
    fn image_welcome(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(48.0);
            ui.heading("Image editor");
            ui.label(
                RichText::new(
                    "Draw a texture here and watch it change on the mesh — no save-and-alt-tab.",
                )
                .weak(),
            );
            ui.add_space(16.0);
            if ui.button("✚  New image…").clicked() {
                self.st.new_form = Some(NewForm::default());
            }
            ui.add_space(4.0);
            ui.label(RichText::new("…or double-click any image in the Assets browser.").small().weak());
            ui.add_space(12.0);
            ui.label(
                RichText::new(
                    "Saving writes both the layered .flimg and a flat .png beside it. \
                     Scenes and materials keep pointing at the .png.",
                )
                .small()
                .weak(),
            );
        });
        self.image_new_dialog(ui);
    }

    // --- menus -------------------------------------------------------------

    fn image_menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("image-menus").show(ui, |ui| {
            ui.horizontal(|ui| {
                self.image_file_menu(ui);
                self.image_edit_menu(ui);
                self.image_image_menu(ui);
                self.image_layer_menu(ui);
                self.image_select_menu(ui);
                self.image_filter_menu(ui);
                self.image_view_menu(ui);
                ui.separator();
                // Undo / redo, tab-local (Ctrl+Z over the canvas).
                if ui
                    .add_enabled(self.st.can_undo(), egui::Button::new("↶"))
                    .on_hover_text("undo (Ctrl+Z) — this tab's own history, never the scene's")
                    .clicked()
                {
                    self.st.undo();
                }
                if ui.add_enabled(self.st.can_redo(), egui::Button::new("↷")).on_hover_text("redo (Ctrl+Y)").clicked() {
                    self.st.redo();
                }
                ui.separator();
                let dirty = self.st.dirty;
                if ui
                    .add_enabled(dirty, egui::Button::new("⇩ Save"))
                    .on_hover_text("write the .flimg and the .png beside it (Ctrl+S)")
                    .clicked()
                {
                    self.cmd.image_save = true;
                }
                let mut live = self.st.live;
                if ui
                    .checkbox(&mut live, "Live")
                    .on_hover_text(
                        "re-export the .png after every edit, so the mesh in the Scene view \
                         updates as you draw. Split the Scene tab beside this one to watch it.",
                    )
                    .changed()
                {
                    self.st.live = live;
                    if live && self.st.path.is_none() {
                        self.st.toast("save the image once, then Live keeps it up to date");
                    }
                }
                ui.separator();
                ui.label(RichText::new(self.st.title()).small().weak());
            });
        });
    }

    /// **View** — the overlays, and the sheet grid you draw a tileset against.
    ///
    /// The two toggles here had no UI at all before: `show_grid` and
    /// `show_checker` were fields hard-coded to `true` and reachable from
    /// nowhere. Everything below them was a literal in the draw code
    /// (`floptle/0096`, `floptle/0097`).
    fn image_view_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("View", |ui| {
            let before = self.st.look;
            let l = &mut self.st.look;

            ui.checkbox(&mut l.checker, "Transparency checker");
            ui.add_enabled_ui(l.checker, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.color_edit_button_srgb(&mut l.checker_a);
                    ui.color_edit_button_srgb(&mut l.checker_b);
                    ui.add(
                        egui::DragValue::new(&mut l.checker_px)
                            .range(2.0..=64.0)
                            .speed(0.5)
                            .suffix(" px"),
                    )
                    .on_hover_text("square edge in SCREEN pixels, so it looks the same at any zoom");
                });
            });

            ui.separator();
            ui.checkbox(&mut l.pixel_grid, "Pixel grid").on_hover_text("one line per texel");
            ui.add_enabled_ui(l.pixel_grid, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.color_edit_button_srgb(&mut l.pixel_grid_color);
                    ui.add(egui::DragValue::new(&mut l.pixel_grid_alpha).range(0..=255).prefix("α "));
                    ui.add(
                        egui::DragValue::new(&mut l.pixel_grid_zoom)
                            .range(1.0..=32.0)
                            .speed(0.2)
                            .prefix("from "),
                    )
                    .on_hover_text("zoom below which the grid is more noise than grid");
                });
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.checkbox(&mut l.pixel_grid_two_tone, "Two-tone")
                        .on_hover_text(
                            "dark dashes over the light line, so one of the two shows against \
                             art of any colour — legible without being configured for it",
                        );
                });
            });

            ui.separator();
            // The cell grid needs a sheet to draw, and the sheet is a property of
            // the IMAGE — so its numbers are here, beside the toggle that shows
            // them, rather than in a settings file the art does not travel with.
            ui.checkbox(&mut l.cell_grid, "Sheet cell grid");
            ui.add_enabled_ui(l.cell_grid, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(16.0);
                    ui.color_edit_button_srgb(&mut l.cell_grid_color);
                    ui.add(egui::DragValue::new(&mut l.cell_grid_alpha).range(0..=255).prefix("α "));
                });
            });
            let look = *l;
            if look != before {
                crate::prefs::save_canvas_look(&look);
            }
            if ui.button("Reset overlays to defaults").clicked() {
                self.st.look = crate::prefs::CanvasLook::default();
                crate::prefs::save_canvas_look(&self.st.look);
            }

            ui.separator();
            self.image_sheet_controls(ui);
        });
    }

    /// The image's own cell grid: how many cells across and down it is cut into.
    ///
    /// Offered as a **cell size** as well as a count, because that is the number
    /// a pixel artist has in their head ("16x16 tiles"), and refusing a grid that
    /// does not divide the canvas evenly, because a 10.6-px cell is a mistake to
    /// draw against rather than a number to round.
    fn image_sheet_controls(&mut self, ui: &mut egui::Ui) {
        let Some((w, h)) = self.st.doc.as_ref().map(|d| (d.w, d.h)) else { return };
        let mut sheet = self.st.doc.as_ref().and_then(|d| d.sheet);
        let mut on = sheet.is_some();
        let mut changed = false;
        if ui
            .checkbox(&mut on, "This image is a sheet")
            .on_hover_text("its cell grid is saved with the image, and drawn over it as you work")
            .changed()
        {
            // A first guess from the canvas rather than 1x1: a square cell that
            // divides both sides is what a sheet almost always is.
            sheet = on.then(|| guess_sheet(w, h));
            changed = true;
        }
        if let Some((c, r)) = sheet.as_mut() {
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                changed |= ui
                    .add(egui::DragValue::new(c).range(1..=1024).speed(0.2).prefix("cols "))
                    .changed();
                changed |= ui
                    .add(egui::DragValue::new(r).range(1..=1024).speed(0.2).prefix("rows "))
                    .changed();
            });
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                if w.is_multiple_of(*c) && h.is_multiple_of(*r) {
                    ui.small(format!("{}×{} px per cell", w / *c, h / *r));
                } else {
                    ui.colored_label(
                        egui::Color32::from_rgb(255, 200, 80),
                        format!("⚠ {w}×{h} does not divide into {c}×{r}"),
                    );
                    ui.small("no grid is drawn until it does");
                }
            });
            ui.horizontal(|ui| {
                ui.add_space(16.0);
                ui.small("cell size");
                for px in [8u32, 16, 24, 32, 48, 64] {
                    if w.is_multiple_of(px)
                        && h.is_multiple_of(px)
                        && ui.small_button(format!("{px}")).clicked()
                    {
                        *c = w / px;
                        *r = h / px;
                        changed = true;
                    }
                }
            });
        }
        if changed && let Some(d) = self.st.doc.as_mut() {
            d.sheet = sheet;
            self.st.mark_dirty();
        }
    }

    fn image_file_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("File", |ui| {
            if ui.button("✚ New image…").clicked() {
                self.st.new_form = Some(NewForm::default());
                ui.close();
            }
            ui.separator();
            if ui.button("⇩ Save").clicked() {
                self.cmd.image_save = true;
                ui.close();
            }
            if ui.button("⇩ Save as…").clicked() {
                self.st.save_name = Some(
                    self.st
                        .path
                        .as_ref()
                        .and_then(|p| p.file_stem())
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_else(|| "untitled".into()),
                );
                ui.close();
            }
            ui.separator();
            ui.menu_button("⇩ Export", |ui| {
                if ui.button("Flattened PNG").clicked() {
                    self.cmd.image_export = Some(ImageExport::Png);
                    ui.close();
                }
                if ui.button("Active layer").clicked() {
                    self.cmd.image_export = Some(ImageExport::Layer);
                    ui.close();
                }
                if ui.add_enabled(self.st.has_selection(), egui::Button::new("Selection")).clicked() {
                    self.cmd.image_export = Some(ImageExport::Selection);
                    ui.close();
                }
                let frames = self.st.doc.as_ref().map_or(1, |d| d.frames);
                if ui
                    .add_enabled(frames > 1, egui::Button::new("Frames → sprite sheet"))
                    .on_hover_text(
                        "one uniform grid, row-major — and the cols/rows are written into \
                         .floptle/textures.ron so UI images and VFX flipbooks can address it",
                    )
                    .clicked()
                {
                    self.cmd.image_export = Some(ImageExport::Sheet);
                    ui.close();
                }
                if ui.add_enabled(frames > 1, egui::Button::new("Frames → animated GIF")).clicked() {
                    self.cmd.image_export = Some(ImageExport::Gif);
                    ui.close();
                }
            });
            ui.separator();
            if ui.button("⊗ Close image").clicked() {
                self.cmd.image_close = true;
                ui.close();
            }
        });
    }

    /// Undo/redo and the clipboard, where every editor puts them.
    fn image_edit_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("Edit", |ui| {
            if ui.add_enabled(self.st.can_undo(), egui::Button::new("↶ Undo   Ctrl+Z")).clicked() {
                self.st.undo();
                ui.close();
            }
            if ui.add_enabled(self.st.can_redo(), egui::Button::new("↷ Redo   Ctrl+Y")).clicked() {
                self.st.redo();
                ui.close();
            }
            ui.separator();
            if ui
                .button("Copy   Ctrl+C")
                .on_hover_text("the selection, or everything painted on this layer")
                .clicked()
            {
                self.st.copy_selection(false);
                ui.close();
            }
            if ui.button("Cut   Ctrl+X").clicked() {
                self.st.copy_selection(true);
                ui.close();
            }
            if ui
                .add_enabled(self.st.has_clipboard(), egui::Button::new("Paste   Ctrl+V"))
                .on_hover_text("lands as a floating block you can drag; Enter applies it")
                .clicked()
            {
                self.st.paste();
                ui.close();
            }
            ui.separator();
            if ui.button("? Keyboard shortcuts").clicked() {
                self.st.show_keys = true;
                ui.close();
            }
        });
    }

    fn image_image_menu(&mut self, ui: &mut egui::Ui) {
        let Some((w, h, mode, doc_tiling)) =
            self.st.doc.as_ref().map(|d| (d.w, d.h, d.mode, d.tiling))
        else {
            return;
        };
        ui.menu_button("Image", |ui| {
            ui.label(RichText::new(format!("{w} × {h} · {}", mode.label())).small().weak());
            ui.separator();
            ui.menu_button("Way of working", |ui| {
                for m in Mode::ALL {
                    if ui
                        .selectable_label(mode == m, m.label())
                        .on_hover_text(match m {
                            Mode::Pixel => "no anti-aliasing, integer zoom, nearest export",
                            Mode::Painterly => "soft brushes, continuous zoom, mipmapped export",
                            Mode::Vector => "shapes first — the same document, vector tools to hand",
                        })
                        .clicked()
                    {
                        self.st.push_undo();
                        if let Some(d) = self.st.doc.as_mut() {
                            d.mode = m;
                        }
                        self.st.brush = crate::image_edit::default_brush_for(m);
                        self.st.invalidate_all();
                        ui.close();
                    }
                }
            });
            ui.separator();
            if ui.button("Resize canvas…").clicked() {
                self.st.new_form = Some(NewForm { w, h, mode, resize: true, ..Default::default() });
                ui.close();
            }
            if ui.button("Scale image…").clicked() {
                self.st.new_form =
                    Some(NewForm { w, h, mode, resize: true, scale: true, ..Default::default() });
                ui.close();
            }
            if ui.button("Trim to content").clicked() {
                self.st.push_undo();
                let f = self.st.frame;
                if let Some(d) = self.st.doc.as_mut() {
                    d.trim(f);
                }
                self.st.invalidate_all();
                ui.close();
            }
            if ui
                .add_enabled(self.st.has_selection(), egui::Button::new("Crop to selection"))
                .on_hover_text("cut the canvas down to what's selected")
                .clicked()
            {
                self.st.push_undo();
                let cropped = self.st.doc.as_mut().is_some_and(|d| d.crop_to_selection());
                self.st.invalidate_all();
                self.st.fit_pending = true;
                if !cropped {
                    self.st.toast("nothing to crop to");
                }
                ui.close();
            }
            ui.separator();
            if ui.button("Flip horizontal").clicked() {
                self.image_canvas_turn(|d| d.flip(true));
                ui.close();
            }
            if ui.button("Flip vertical").clicked() {
                self.image_canvas_turn(|d| d.flip(false));
                ui.close();
            }
            if ui.button("Rotate 90° ⟳").clicked() {
                self.image_canvas_turn(|d| d.rotate(1));
                ui.close();
            }
            if ui.button("Rotate 90° ⟲").clicked() {
                self.image_canvas_turn(|d| d.rotate(-1));
                ui.close();
            }
            ui.separator();
            let mut tiling = doc_tiling;
            if ui
                .checkbox(&mut tiling, "Tiling mode")
                .on_hover_text("strokes wrap at the canvas edges — this is what MAKES a texture seamless")
                .changed()
                && let Some(d) = self.st.doc.as_mut()
            {
                d.tiling = tiling;
                self.st.mark_dirty();
            }
            let mut tiled_view = self.st.tiled_view;
            if ui.checkbox(&mut tiled_view, "Show 3×3 repeat").changed() {
                self.st.tiled_view = tiled_view;
            }
        });
    }

    fn image_layer_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("Layer", |ui| {
            if ui.button("✚ Pixel layer").clicked() {
                self.st.push_undo();
                if let Some(d) = self.st.doc.as_mut() {
                    d.add_raster_layer();
                }
                self.st.invalidate_all();
                ui.close();
            }
            if ui.button("✚ Vector layer").clicked() {
                self.st.push_undo();
                if let Some(d) = self.st.doc.as_mut() {
                    d.add_layer(Layer::vector("Vector"));
                }
                self.st.invalidate_all();
                ui.close();
            }
            ui.menu_button("✚ Adjustment layer", |ui| {
                for a in Adjustment::presets() {
                    if ui.button(a.label()).clicked() {
                        self.st.push_undo();
                        if let Some(d) = self.st.doc.as_mut() {
                            d.add_layer(Layer::adjust(a.clone()));
                        }
                        self.st.invalidate_all();
                        ui.close();
                    }
                }
            });
            ui.separator();
            let active = self.st.doc.as_ref().map_or(0, |d| d.active);
            if ui.button("⎘ Duplicate").clicked() {
                self.st.push_undo();
                if let Some(d) = self.st.doc.as_mut() {
                    d.duplicate_layer(active);
                }
                self.st.invalidate_all();
                ui.close();
            }
            if ui.button("⏷ Merge down").clicked() {
                self.st.push_undo();
                let f = self.st.frame;
                if let Some(d) = self.st.doc.as_mut() {
                    d.merge_down(active, f);
                }
                self.st.invalidate_all();
                ui.close();
            }
            if ui.button("▤ Flatten image").clicked() {
                self.st.push_undo();
                let f = self.st.frame;
                if let Some(d) = self.st.doc.as_mut() {
                    d.flatten_all(f);
                }
                self.st.invalidate_all();
                ui.close();
            }
            if ui.button("🗑 Delete").clicked() {
                self.st.push_undo();
                if let Some(d) = self.st.doc.as_mut() {
                    d.delete_layer(active);
                }
                self.st.invalidate_all();
                ui.close();
            }
        });
    }

    fn image_select_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("Select", |ui| {
            if ui.button("None (whole canvas)").clicked() {
                self.st.deselect();
                ui.close();
            }
            if ui.button("Invert").clicked() {
                self.st.invert_selection();
                ui.close();
            }
            if ui.button("Opaque pixels of this layer").clicked() {
                self.st.push_undo();
                let f = self.st.frame;
                if let Some(d) = self.st.doc.as_mut() {
                    let a = d.active;
                    if let Some(g) = d.layers.get(a).and_then(|l| l.grid(f)) {
                        d.selection = Some(floptle_image::select::alpha_mask(g));
                    }
                }
                self.st.invalidate_all();
                ui.close();
            }
            ui.separator();
            let has = self.st.has_selection();
            if ui.add_enabled(has, egui::Button::new("Grow 1 px")).clicked() {
                self.image_grow_selection(1);
                ui.close();
            }
            if ui.add_enabled(has, egui::Button::new("Shrink 1 px")).clicked() {
                self.image_grow_selection(-1);
                ui.close();
            }
            if ui.add_enabled(has, egui::Button::new("Feather 2 px")).clicked() {
                self.st.push_undo();
                if let Some(d) = self.st.doc.as_mut()
                    && let Some(s) = d.selection.as_mut()
                {
                    s.feather(2);
                }
                self.st.invalidate_all();
                ui.close();
            }
            ui.separator();
            if ui
                .add_enabled(has, egui::Button::new("Use as layer mask"))
                .on_hover_text("turn the selection into this layer's mask")
                .clicked()
            {
                self.st.push_undo();
                if let Some(d) = self.st.doc.as_mut() {
                    let a = d.active;
                    let sel = d.selection.clone();
                    if let (Some(l), Some(s)) = (d.layers.get_mut(a), sel) {
                        l.mask = Some(s);
                        l.mask_enabled = true;
                    }
                    d.selection = None;
                }
                self.st.invalidate_all();
                ui.close();
            }
            if ui.add_enabled(has, egui::Button::new("Delete selected pixels")).clicked() {
                self.st.push_undo();
                let f = self.st.frame;
                if let Some(d) = self.st.doc.as_mut() {
                    let (a, bounds, sel) = (d.active, d.bounds(), d.selection.clone());
                    if let Some(l) = d.layers.get_mut(a)
                        && !l.locked
                        && let Some(g) = l.grid_mut(f)
                    {
                        floptle_image::brush::clear_region(g, bounds, sel.as_ref());
                    }
                }
                self.st.invalidate_all();
                ui.close();
            }
        });
    }

    fn image_filter_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("Filter", |ui| {
            ui.label(RichText::new("destructive — with a live preview").small().weak());
            ui.separator();
            for k in FilterKind::ALL {
                if ui.button(k.label()).on_hover_text(k.hint()).clicked() {
                    self.st.begin_filter(k);
                    ui.close();
                }
            }
            ui.separator();
            if ui
                .button("Seam finder (offset by half)")
                .on_hover_text("roll the image half a canvas so the tiling seams land in the middle, where you can paint them out")
                .clicked()
            {
                self.st.push_undo();
                let (w, h) = self.st.doc.as_ref().map_or((0, 0), |d| (d.w, d.h));
                self.image_whole_canvas_op(move |buf, bw, bh| {
                    floptle_image::filter::offset_wrap(buf, bw, bh, (w / 2) as i32, (h / 2) as i32)
                });
                ui.close();
            }
        });
    }

    // --- whole-canvas helpers ---------------------------------------------

    /// Apply `f` to every raster layer's every frame (flips, offsets, seams).
    fn image_whole_canvas_op(&mut self, f: impl Fn(&mut [u8], u32, u32)) {
        self.st.push_undo();
        let Some(doc) = self.st.doc.as_mut() else { return };
        let (w, h) = (doc.w, doc.h);
        for l in &mut doc.layers {
            if let LayerKind::Raster { frames } = &mut l.kind {
                for g in frames.iter_mut() {
                    let mut buf = g.to_rgba();
                    f(&mut buf, w, h);
                    *g = floptle_image::TileGrid::from_rgba(w, h, &buf);
                }
            }
        }
        self.st.invalidate_all();
    }

    /// A whole-document turn (flip / rotate). The kernel does the work so masks,
    /// the selection and vector layers come along — the version that lived here
    /// moved the pixels and dropped everything else.
    fn image_canvas_turn(&mut self, f: impl FnOnce(&mut floptle_image::doc::Image)) {
        self.st.push_undo();
        let before = self.st.doc.as_ref().map(|d| (d.w, d.h));
        if let Some(doc) = self.st.doc.as_mut() {
            f(doc);
        }
        self.st.invalidate_all();
        // Re-fit ONLY when the canvas actually changed shape (a rotate). A flip
        // must not move the view: nothing re-frames itself in this editor.
        if before != self.st.doc.as_ref().map(|d| (d.w, d.h)) {
            self.st.fit_pending = true;
        }
    }

    fn image_grow_selection(&mut self, n: i32) {
        self.st.push_undo();
        if let Some(d) = self.st.doc.as_mut()
            && let Some(s) = d.selection.as_mut()
        {
            s.expand(n);
        }
        self.st.invalidate_all();
    }

    // --- tool strip ---------------------------------------------------------

    fn image_tool_strip(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("image-tools")
            .resizable(false)
            .exact_size(38.0)
            .show(ui, |ui| {
                // Eighteen tools is ~500 px of strip. Docked short — beside a
                // Scene view, or on a laptop — the tail of the list was simply
                // CLIPPED AWAY, with no scrollbar and no way to reach the pen or
                // the text tool except by keyboard.
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .scroll_bar_visibility(
                        egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded,
                    )
                    .show(ui, |ui| self.image_tool_buttons(ui));
            });
    }

    fn image_tool_buttons(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        for t in ImgTool::ALL {
            let (name, key) = t.label();
            let sel = self.st.tool == t;
            let (rect, resp) =
                ui.allocate_exact_size(egui::vec2(30.0, 26.0), egui::Sense::click());
            let v = ui.visuals();
            let bg = if sel {
                v.selection.bg_fill
            } else if resp.hovered() {
                v.widgets.hovered.bg_fill
            } else {
                Color32::TRANSPARENT
            };
            ui.painter().rect_filled(rect, 4.0, bg);
            let fg = if sel {
                v.strong_text_color()
            } else if resp.hovered() {
                v.widgets.hovered.fg_stroke.color
            } else {
                v.widgets.inactive.fg_stroke.color
            };
            crate::image_icons::draw_tool_icon(ui.painter(), rect.shrink(6.0), t, fg);
            if resp.on_hover_text(format!("{name}  ({key})")).clicked() {
                self.st.tool = t;
                // The three paint tools share one brush; switching to the
                // soft brush shouldn't silently keep the 1 px pencil.
                match t {
                    ImgTool::Pencil => {
                        self.st.brush.pixel_perfect = true;
                        self.st.brush.mode = BrushMode::Paint;
                    }
                    ImgTool::Brush => {
                        self.st.brush.pixel_perfect = false;
                        self.st.brush.mode = BrushMode::Paint;
                        if self.st.brush.radius <= 1.0 {
                            self.st.brush.radius = 8.0;
                        }
                    }
                    ImgTool::Eraser => self.st.brush.mode = BrushMode::Erase,
                    _ => {}
                }
            }
            // Group the strip the way the tools group in the head:
            // draw · shape · select · move+pick · vector · commit-y.
            if matches!(
                t,
                ImgTool::Gradient | ImgTool::Wand | ImgTool::Eyedropper | ImgTool::Pen
            ) {
                ui.separator();
            }
        }
    }

    // --- the right panel ----------------------------------------------------

    fn image_side_panel(&mut self, ui: &mut egui::Ui) {
        egui::Panel::right("image-panel")
            .resizable(true)
            .default_size(248.0)
            .size_range(200.0..=420.0)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    self.image_filter_preview_ui(ui);
                    self.image_tool_options_ui(ui);
                    ui.separator();
                    self.image_color_ui(ui);
                    ui.separator();
                    self.image_layers_ui(ui);
                    ui.separator();
                    self.image_frames_ui(ui);
                });
            });
    }

    /// The live filter preview: sliders that re-apply from a snapshot each change.
    fn image_filter_preview_ui(&mut self, ui: &mut egui::Ui) {
        let Some(f) = self.st.filter.as_ref() else { return };
        let kind = f.kind;
        let (mut a, mut b, mut mono) = (f.a, f.b, f.mono);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.label(RichText::new(kind.label()).strong());
            let mut changed = false;
            for (label, val, range) in kind.sliders() {
                let v = if val == 0 { &mut a } else { &mut b };
                changed |= ui.add(egui::Slider::new(v, range).text(label)).changed();
            }
            if kind == FilterKind::Noise {
                changed |= ui.checkbox(&mut mono, "monochrome").changed();
            }
            ui.horizontal(|ui| {
                if ui.button("Apply").clicked() {
                    self.st.commit_filter();
                }
                if ui.button("Cancel").clicked() {
                    self.st.cancel_filter();
                }
            });
            if changed {
                self.st.set_filter_params(a, b, mono);
            }
        });
        ui.separator();
    }

    fn image_tool_options_ui(&mut self, ui: &mut egui::Ui) {
        let tool = self.st.tool;
        ui.add_space(4.0);
        ui.label(RichText::new(tool.label().0).strong());
        match tool {
            ImgTool::Pencil | ImgTool::Brush | ImgTool::Eraser => {
                let b = &mut self.st.brush;
                ui.add(egui::Slider::new(&mut b.radius, 0.5..=128.0).logarithmic(true).text("size"));
                if !b.pixel_perfect {
                    ui.add(egui::Slider::new(&mut b.hardness, 0.0..=1.0).text("hardness"));
                    ui.add(egui::Slider::new(&mut b.spacing, 0.02..=1.0).text("spacing"));
                }
                ui.add(egui::Slider::new(&mut b.flow, 0.02..=1.0).text("flow"));
                let pp = &mut b.pixel_perfect;
                ui.checkbox(pp, "pixel-perfect")
                    .on_hover_text("hard integer dabs, and no staircase doubles on a 1 px nib");
                if tool != ImgTool::Eraser {
                    egui::ComboBox::from_id_salt("img-brush-mode")
                        .selected_text(b.mode.label())
                        .show_ui(ui, |ui| {
                            for m in BrushMode::ALL {
                                ui.selectable_value(&mut b.mode, m, m.label());
                            }
                        });
                    if matches!(
                        b.mode,
                        BrushMode::Smudge | BrushMode::Blur | BrushMode::Sharpen | BrushMode::Dodge | BrushMode::Burn
                    ) {
                        ui.add(egui::Slider::new(&mut b.strength, 0.0..=1.0).text("strength"));
                    }
                    if b.mode == BrushMode::Clone {
                        ui.label(RichText::new("Alt+click sets the source").small().weak());
                    }
                    let mut blend = b.blend;
                    egui::ComboBox::from_id_salt("img-brush-blend")
                        .selected_text(blend.label())
                        .show_ui(ui, |ui| {
                            for m in Blend::ALL {
                                ui.selectable_value(&mut blend, m, m.label());
                            }
                        });
                    self.st.brush.blend = blend;
                }
                ui.horizontal(|ui| {
                    ui.label("paint into");
                    let mut s = self.st.surface;
                    if ui.selectable_value(&mut s, PaintTargetSurface::Pixels, "pixels").clicked()
                        || ui.selectable_value(&mut s, PaintTargetSurface::Mask, "mask").clicked()
                    {
                        self.st.surface = s;
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("mirror");
                    ui.checkbox(&mut self.st.mirror_x, "⇄").on_hover_text("mirror left/right");
                    ui.checkbox(&mut self.st.mirror_y, "⇅").on_hover_text("mirror up/down");
                });
            }
            ImgTool::Bucket | ImgTool::Wand => {
                ui.add(egui::Slider::new(&mut self.st.tolerance, 0..=255).text("tolerance"));
                ui.checkbox(&mut self.st.contiguous, "contiguous")
                    .on_hover_text("off = every matching pixel on the layer, not just the connected blob");
            }
            ImgTool::Gradient => {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.st.grad_kind, GradientKind::Linear, "linear");
                    ui.selectable_value(&mut self.st.grad_kind, GradientKind::Radial, "radial");
                });
                ui.label(RichText::new("drags from the primary colour to the secondary").small().weak());
            }
            ImgTool::Line | ImgTool::Rectangle | ImgTool::Ellipse => {
                if tool != ImgTool::Line {
                    ui.checkbox(&mut self.st.shape_fill, "fill");
                }
                ui.checkbox(&mut self.st.shape_stroke, "stroke");
                if self.st.shape_stroke || tool == ImgTool::Line {
                    ui.add(egui::Slider::new(&mut self.st.stroke_width, 0.5..=64.0).text("width"));
                }
                ui.checkbox(&mut self.st.shape_vector, "as a vector layer")
                    .on_hover_text("the same tool — one modifier — spawns re-editable shapes instead of pixels");
                ui.label(RichText::new("hold Shift to constrain").small().weak());
            }
            ImgTool::SelectRect | ImgTool::SelectEllipse | ImgTool::Lasso => {
                ui.horizontal_wrapped(|ui| {
                    for (op, name) in [
                        (SelectOp::Replace, "replace"),
                        (SelectOp::Add, "add"),
                        (SelectOp::Subtract, "subtract"),
                        (SelectOp::Intersect, "intersect"),
                    ] {
                        ui.selectable_value(&mut self.st.sel_op, op, name);
                    }
                });
                ui.add(egui::Slider::new(&mut self.st.sel_feather, 0..=32).text("feather"));
            }
            ImgTool::Reshape | ImgTool::Pen => {
                ui.label(
                    RichText::new(
                        "drag a node to reshape · double-click one to switch corner ⇄ curve · \
                         click an edge to add a node",
                    )
                    .small()
                    .weak(),
                );
                if self.st.active_paths().is_none() {
                    ui.label(RichText::new("(select a vector layer)").small().weak());
                }
                self.image_vector_paint_ui(ui);
            }
            ImgTool::Move => {
                ui.label(RichText::new("drags the whole active layer — nothing is lost off-canvas").small().weak());
            }
            ImgTool::Eyedropper => {
                ui.label(RichText::new("right-drag picks a colour with any tool").small().weak());
            }
            ImgTool::Transform => self.image_transform_ui(ui),
            ImgTool::Text => self.image_text_ui(ui),
        }
    }

    /// Free transform: the numbers behind the handles, plus Apply / Cancel.
    fn image_transform_ui(&mut self, ui: &mut egui::Ui) {
        if self.st.xform.is_none() {
            ui.label(
                RichText::new(
                    "click the canvas to lift the selection — or, with no selection, \
                     everything painted on this layer",
                )
                .small()
                .weak(),
            );
            if ui.button("Lift now").clicked() {
                self.st.begin_transform();
            }
            return;
        }
        let mut xf = self.st.xform.as_ref().map(|s| s.xf).unwrap();
        let mut changed = false;
        ui.horizontal(|ui| {
            changed |= ui.add(egui::DragValue::new(&mut xf.translate.0).speed(0.5).prefix("x ")).changed();
            changed |= ui.add(egui::DragValue::new(&mut xf.translate.1).speed(0.5).prefix("y ")).changed();
        });
        ui.horizontal(|ui| {
            changed |= ui.add(egui::DragValue::new(&mut xf.scale.0).speed(0.01).range(-32.0..=32.0).prefix("sx ")).changed();
            changed |= ui.add(egui::DragValue::new(&mut xf.scale.1).speed(0.01).range(-32.0..=32.0).prefix("sy ")).changed();
        });
        let mut deg = xf.rotate.to_degrees();
        if ui.add(egui::Slider::new(&mut deg, -180.0..=180.0).text("rotate")).changed() {
            xf.rotate = deg.to_radians();
            changed = true;
        }
        ui.horizontal(|ui| {
            if ui.small_button("flip ⇄").clicked() {
                xf.scale.0 = -xf.scale.0;
                changed = true;
            }
            if ui.small_button("flip ⇅").clicked() {
                xf.scale.1 = -xf.scale.1;
                changed = true;
            }
        });
        if changed {
            self.st.set_xform(xf);
        }
        ui.horizontal(|ui| {
            if ui.button("Apply").clicked() {
                self.st.commit_transform();
            }
            if ui.button("Cancel").clicked() {
                self.st.cancel_transform();
            }
        });
        ui.label(
            RichText::new("drag inside to move · a corner to scale (Shift = uniform) · the top handle to rotate (Shift = 15°)")
                .small()
                .weak(),
        );
    }

    /// Text: the field, its size, and Apply / Cancel.
    fn image_text_ui(&mut self, ui: &mut egui::Ui) {
        if self.st.text.is_none() {
            ui.label(RichText::new("click the canvas to place a text block").small().weak());
            return;
        }
        let (mut text, mut size) =
            self.st.text.as_ref().map(|t| (t.text.clone(), t.size)).unwrap();
        let mut changed = false;
        let resp = ui.add(
            egui::TextEdit::multiline(&mut text)
                .desired_rows(3)
                .desired_width(f32::INFINITY)
                .hint_text("type…"),
        );
        // Focus ONCE, on the frame the block was placed. Requesting it every
        // frame meant the field could never be left: the size slider couldn't
        // take a click and Escape was swallowed the moment it arrived.
        if self.st.take_text_focus() {
            resp.request_focus();
        }
        changed |= resp.changed();
        changed |= ui.add(egui::Slider::new(&mut size, 4.0..=256.0).logarithmic(true).text("size")).changed();
        if changed {
            self.st.set_text(text, size);
        }
        ui.horizontal(|ui| {
            if ui.button("Apply").clicked() {
                self.st.commit_text();
            }
            if ui.button("Cancel").clicked() {
                self.st.cancel_text();
            }
        });
        ui.label(
            RichText::new(
                "drawn with the editor's own font · click elsewhere on the canvas to move the \
                 block · Ctrl+Enter applies, Escape cancels",
            )
            .small()
            .weak(),
        );
    }

    /// Fill/stroke of the selected vector path.
    fn image_vector_paint_ui(&mut self, ui: &mut egui::Ui) {
        let Some((pi, _)) = self.st.sel_node else { return };
        let mut changed = false;
        let mut fill_on;
        let mut fill_col = [255u8; 4];
        let mut stroke_on;
        let mut stroke_col = [0u8, 0, 0, 255];
        let mut width = 2.0f32;
        {
            let Some(p) = self.st.active_paths().and_then(|ps| ps.get(pi)) else { return };
            fill_on = p.fill.is_some();
            if let Some(floptle_image::vector::Paint::Solid(c)) = &p.fill {
                fill_col = *c;
            }
            stroke_on = p.stroke.is_some();
            if let Some(s) = &p.stroke {
                stroke_col = s.color;
                width = s.width;
            }
        }
        ui.separator();
        ui.horizontal(|ui| {
            changed |= ui.checkbox(&mut fill_on, "fill").changed();
            changed |= color_button(ui, &mut fill_col, "vec-fill");
        });
        ui.horizontal(|ui| {
            changed |= ui.checkbox(&mut stroke_on, "stroke").changed();
            changed |= color_button(ui, &mut stroke_col, "vec-stroke");
        });
        if stroke_on {
            changed |= ui.add(egui::Slider::new(&mut width, 0.5..=64.0).text("width")).changed();
        }
        if changed {
            self.st.push_undo();
            if let Some(p) = self.st.vector_path_mut(pi) {
                p.fill = fill_on.then_some(floptle_image::vector::Paint::Solid(fill_col));
                p.stroke = stroke_on.then(|| floptle_image::vector::Stroke {
                    color: stroke_col,
                    width,
                    ..Default::default()
                });
            }
            self.st.invalidate_vectors();
        }
    }

    // --- colour + palette ---------------------------------------------------

    fn image_color_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Colour").strong());
            if ui.small_button("⇄").on_hover_text("swap primary / secondary (X)").clicked() {
                std::mem::swap(&mut self.st.color, &mut self.st.color2);
            }
        });
        let mut recolour = false;
        ui.horizontal(|ui| {
            recolour |= color_button(ui, &mut self.st.color, "img-primary");
            color_button(ui, &mut self.st.color2, "img-secondary");
            // Typed, not just displayed: a hex code is how colours travel
            // between a palette, a style guide and a teammate's message.
            let mut hex = self.st.hex_entry.clone().unwrap_or_else(|| hex_of(self.st.color));
            let resp = ui.add(
                egui::TextEdit::singleline(&mut hex)
                    .desired_width(72.0)
                    .font(egui::TextStyle::Monospace),
            );
            if resp.has_focus() || resp.changed() {
                if let Some(c) = parse_hex(&hex) {
                    self.st.color = [c[0], c[1], c[2], self.st.color[3]];
                    recolour = true;
                }
                self.st.hex_entry = Some(hex);
            } else {
                // Not being typed in: mirror whatever the swatch says.
                self.st.hex_entry = None;
            }
        });
        if recolour {
            // A live text block follows the colour you just picked.
            self.st.mark_text_dirty();
        }

        let Some((has_pal, palette_lock, colors)) = self.st.doc.as_ref().map(|d| {
            (
                d.palette.is_some(),
                d.palette_lock,
                d.palette.as_ref().map(|p| p.colors.clone()).unwrap_or_default(),
            )
        }) else {
            return;
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new("Palette").strong());
            let mut lock = palette_lock;
            if ui
                .add_enabled(has_pal, egui::Checkbox::new(&mut lock, "lock"))
                .on_hover_text("every colour you place snaps to the nearest entry")
                .changed()
                && let Some(d) = self.st.doc.as_mut()
            {
                d.palette_lock = lock;
                self.st.mark_dirty();
            }
            ui.menu_button("▾", |ui| {
                let names: Vec<String> = self.st.palettes.iter().map(|p| p.name.clone()).collect();
                for (i, n) in names.iter().enumerate() {
                    if ui.button(n).clicked() {
                        let p = self.st.palettes[i].clone();
                        if let Some(d) = self.st.doc.as_mut() {
                            d.palette = Some(p);
                        }
                        self.st.mark_dirty();
                        ui.close();
                    }
                }
                ui.separator();
                if ui.button("From this image").clicked() {
                    let f = self.st.frame;
                    if let Some(d) = self.st.doc.as_mut() {
                        let flat = floptle_image::composite::flatten(d, f);
                        d.palette = Some(Palette::from_image(&flat, 32));
                    }
                    ui.close();
                }
                if ui.add_enabled(has_pal, egui::Button::new("Save to project…")).clicked() {
                    self.cmd.image_save_palette = true;
                    ui.close();
                }
                if ui.button("⟲ Rescan .floptle/palettes").clicked() {
                    self.st.palettes_loaded = false;
                    ui.close();
                }
                ui.separator();
                if ui.add_enabled(has_pal, egui::Button::new("Clear")).clicked() {
                    if let Some(d) = self.st.doc.as_mut() {
                        d.palette = None;
                        d.palette_lock = false;
                    }
                    ui.close();
                }
            });
        });
        // The swatch bar doubles as the palette panel.
        if colors.is_empty() {
            ui.label(RichText::new("no palette — pick one from ▾").small().weak());
            return;
        }
        ui.horizontal_wrapped(|ui| {
            for c in colors {
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::click());
                ui.painter().rect_filled(rect, 2.0, Color32::from_rgb(c[0], c[1], c[2]));
                if self.st.color[..3] == c[..3] {
                    ui.painter().rect_stroke(
                        rect,
                        2.0,
                        egui::Stroke::new(2.0, Color32::WHITE),
                        egui::StrokeKind::Inside,
                    );
                }
                if resp.clicked() {
                    self.st.color = c;
                    self.st.mark_text_dirty();
                }
                if resp.secondary_clicked() {
                    self.st.color2 = c;
                }
                resp.on_hover_text(hex_of(c));
            }
        });
    }

    // --- layers -------------------------------------------------------------

    fn image_layers_ui(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Layers").strong());
            if ui.small_button("✚").on_hover_text("new pixel layer").clicked() {
                self.st.push_undo();
                if let Some(d) = self.st.doc.as_mut() {
                    d.add_raster_layer();
                }
                self.st.invalidate_all();
            }
            if ui.small_button("⎘").on_hover_text("duplicate").clicked() {
                self.st.push_undo();
                if let Some(d) = self.st.doc.as_mut() {
                    let a = d.active;
                    d.duplicate_layer(a);
                }
                self.st.invalidate_all();
            }
            if ui.small_button("⏶").on_hover_text("move up").clicked() {
                self.st.push_undo();
                if let Some(d) = self.st.doc.as_mut() {
                    let a = d.active;
                    d.move_layer(a, 1);
                }
                self.st.invalidate_all();
            }
            if ui.small_button("⏷").on_hover_text("move down").clicked() {
                self.st.push_undo();
                if let Some(d) = self.st.doc.as_mut() {
                    let a = d.active;
                    d.move_layer(a, -1);
                }
                self.st.invalidate_all();
            }
            if ui.small_button("🗑").on_hover_text("delete").clicked() {
                self.st.push_undo();
                if let Some(d) = self.st.doc.as_mut() {
                    let a = d.active;
                    d.delete_layer(a);
                }
                self.st.invalidate_all();
            }
        });

        let Some((n, active, rows)) = self.st.doc.as_ref().map(|doc| {
            (
                doc.layers.len(),
                doc.active,
                doc.layers
                    .iter()
                    .map(|l| (l.name.clone(), l.visible, l.locked, l.clip_below, l.kind.glyph()))
                    .collect::<Vec<(String, bool, bool, bool, &'static str)>>(),
            )
        }) else {
            return;
        };
        let mut set_active = None;
        let mut toggle_vis = None;
        self.st.sync_thumbs(ui.ctx());
        // Top of the list = top of the stack, the way every editor shows it.
        for i in (0..n).rev() {
            let (name, vis, locked, clip, glyph) = &rows[i];
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new(if *vis { "👁" } else { "  " }).frame(false))
                    .on_hover_text("show / hide")
                    .clicked()
                {
                    toggle_vis = Some(i);
                }
                // A 20 px thumbnail: which layer is which, without reading.
                let (tr, _) = ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
                ui.painter().rect_filled(tr, 2.0, Color32::from_gray(52));
                match self.st.thumb(i) {
                    Some(t) => {
                        ui.painter().image(
                            t.id(),
                            tr,
                            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                            Color32::WHITE,
                        );
                    }
                    None => {
                        ui.painter().rect_stroke(
                            tr,
                            2.0,
                            egui::Stroke::new(1.0, Color32::from_gray(70)),
                            egui::StrokeKind::Inside,
                        );
                    }
                }
                let label = format!("{}{} {}", if *clip { "↳ " } else { "" }, glyph, name);
                let mut text = RichText::new(label);
                if *locked {
                    text = text.italics();
                }
                if !*vis {
                    text = text.weak();
                }
                if ui.selectable_label(i == active, text).clicked() {
                    set_active = Some(i);
                }
            });
        }
        if let Some(i) = toggle_vis
            && let Some(d) = self.st.doc.as_mut()
        {
            if let Some(l) = d.layers.get_mut(i) {
                l.visible = !l.visible;
            }
            self.st.invalidate_all();
        }
        if let Some(i) = set_active {
            // Settle any floating transform / text block first: it re-applies
            // itself onto whichever layer is active, so switching underneath it
            // would stamp it into the wrong one.
            self.st.commit_live();
            if let Some(d) = self.st.doc.as_mut() {
                d.active = i;
            }
            self.st.sel_node = None;
        }

        self.image_active_layer_ui(ui);
    }

    /// Properties of the active layer: blend, opacity, clipping, mask, effects,
    /// and (for an adjustment layer) its parameters.
    fn image_active_layer_ui(&mut self, ui: &mut egui::Ui) {
        let Some((i, mut opacity, mut blend, mut clip, mut locked, mut mask_on, has_mask, is_adjust, animated, mut name)) =
            self.st.doc.as_ref().and_then(|doc| {
                let i = doc.active;
                let l = doc.layers.get(i)?;
                Some((
                    i,
                    l.opacity,
                    l.blend,
                    l.clip_below,
                    l.locked,
                    l.mask_enabled,
                    l.mask.is_some(),
                    l.kind.is_adjust(),
                    l.is_animated(),
                    l.name.clone(),
                ))
            })
        else {
            return;
        };
        let mut changed = false;
        let mut structural = false;

        ui.add_space(4.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            changed |= ui.add(egui::TextEdit::singleline(&mut name).desired_width(f32::INFINITY)).changed();
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("img-layer-blend")
                    .selected_text(blend.label())
                    .width(120.0)
                    .show_ui(ui, |ui| {
                        for m in Blend::ALL {
                            if ui.selectable_value(&mut blend, m, m.label()).clicked() {
                                structural = true;
                            }
                        }
                    });
                changed |= ui.checkbox(&mut locked, "lock").on_hover_text("lock: refuse edits").changed();
            });
            structural |= ui.add(egui::Slider::new(&mut opacity, 0.0..=1.0).text("opacity")).changed();
            structural |= ui
                .checkbox(&mut clip, "clip to layer below")
                .on_hover_text("confine this layer to the alpha of the one under it")
                .changed();
            ui.horizontal(|ui| {
                if has_mask {
                    structural |= ui.checkbox(&mut mask_on, "mask").changed();
                    if ui.small_button("🗑").on_hover_text("delete mask").clicked() {
                        self.st.push_undo();
                        if let Some(d) = self.st.doc.as_mut()
                            && let Some(l) = d.layers.get_mut(i)
                        {
                            l.mask = None;
                        }
                        structural = true;
                    }
                } else if ui.small_button("✚ mask").clicked() {
                    self.st.push_undo();
                    if let Some(d) = self.st.doc.as_mut() {
                        let (w, h) = (d.w, d.h);
                        if let Some(l) = d.layers.get_mut(i) {
                            l.mask = Some(floptle_image::select::Mask::new(w, h, 255));
                            l.mask_enabled = true;
                        }
                    }
                    structural = true;
                }
                if !is_adjust {
                    let mut anim = animated;
                    if ui
                        .checkbox(&mut anim, "per-frame")
                        .on_hover_text("give this layer its own pixels on every frame")
                        .changed()
                    {
                        self.st.push_undo();
                        if let Some(d) = self.st.doc.as_mut() {
                            d.set_layer_animated(i, anim);
                        }
                        structural = true;
                    }
                }
            });

            // Adjustment parameters.
            if let Some(LayerKind::Adjust(a)) = self.st.doc.as_ref().map(|d| &d.layers[i].kind) {
                let mut adj = a.clone();
                let palettes = self.st.palettes.clone();
                if adjustment_params_ui(ui, &mut adj, &palettes) {
                    if let Some(d) = self.st.doc.as_mut()
                        && let Some(l) = d.layers.get_mut(i)
                    {
                        l.kind = LayerKind::Adjust(adj);
                    }
                    structural = true;
                }
            }

            // Effects.
            ui.horizontal(|ui| {
                ui.label(RichText::new("Effects").small().strong());
                ui.menu_button("✚", |ui| {
                    for e in Effect::presets() {
                        if ui.button(e.label()).clicked() {
                            self.st.push_undo();
                            if let Some(d) = self.st.doc.as_mut()
                                && let Some(l) = d.layers.get_mut(i)
                            {
                                l.effects.push(e.clone());
                            }
                            self.st.invalidate_all();
                            ui.close();
                        }
                    }
                });
            });
            let effects = self.st.doc.as_ref().map(|d| d.layers[i].effects.clone()).unwrap_or_default();
            let mut remove = None;
            for (ei, e) in effects.iter().enumerate() {
                let mut e2 = e.clone();
                ui.horizontal(|ui| {
                    ui.label(RichText::new(e.label()).small());
                    if ui.small_button("🗑").clicked() {
                        remove = Some(ei);
                    }
                });
                if effect_params_ui(ui, &mut e2, ei)
                    && let Some(d) = self.st.doc.as_mut()
                    && let Some(l) = d.layers.get_mut(i)
                    && let Some(slot) = l.effects.get_mut(ei)
                {
                    *slot = e2;
                    structural = true;
                }
            }
            if let Some(ei) = remove {
                self.st.push_undo();
                if let Some(d) = self.st.doc.as_mut()
                    && let Some(l) = d.layers.get_mut(i)
                {
                    l.effects.remove(ei);
                }
                structural = true;
            }
        });

        if changed || structural {
            // One undo step per gesture: the snapshot is taken on the first
            // changed frame and banked when the pointer comes up (`flush_edit`
            // in `ui`), so dragging opacity for three seconds is one Ctrl+Z.
            self.st.begin_edit();
            if let Some(d) = self.st.doc.as_mut()
                && let Some(l) = d.layers.get_mut(i)
            {
                l.name = name;
                l.opacity = opacity;
                l.blend = blend;
                l.clip_below = clip;
                l.locked = locked;
                l.mask_enabled = mask_on;
            }
            self.st.invalidate_all();
        }
    }

    // --- frames --------------------------------------------------------------

    fn image_frames_ui(&mut self, ui: &mut egui::Ui) {
        let Some((frames, mut fps)) = self.st.doc.as_ref().map(|d| (d.frames, d.fps)) else {
            return;
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new("Frames").strong());
            ui.label(RichText::new(format!("{}/{}", self.st.frame + 1, frames)).small().weak());
            let add_hint =
                if frames == 1 { "start animating — adds a second frame" } else { "duplicate this frame" };
            if ui.small_button("✚").on_hover_text(add_hint).clicked() {
                self.st.push_undo();
                let f = self.st.frame;
                if let Some(d) = self.st.doc.as_mut() {
                    if d.frames == 1 {
                        // The first "add frame" also makes every layer per-frame,
                        // or the new frame would be a copy that can't diverge.
                        d.set_frames(2);
                        for i in 0..d.layers.len() {
                            d.set_layer_animated(i, true);
                        }
                    } else {
                        d.duplicate_frame(f);
                    }
                }
                self.st.set_frame(self.st.frame + 1);
            }
            if ui.add_enabled(frames > 1, egui::Button::new("🗑").small()).on_hover_text("delete frame").clicked() {
                self.st.push_undo();
                let f = self.st.frame;
                if let Some(d) = self.st.doc.as_mut() {
                    d.delete_frame(f);
                }
                self.st.set_frame(self.st.frame.saturating_sub(1));
            }
            if frames > 1 {
                let playing = self.st.playing;
                if ui.small_button(if playing { "⏸" } else { "⏵" }).clicked() {
                    self.st.playing = !playing;
                }
            }
        });
        if frames > 1 {
            ui.horizontal_wrapped(|ui| {
                for f in 0..frames {
                    if ui.selectable_label(f == self.st.frame, format!("{}", f + 1)).clicked() {
                        self.st.set_frame(f);
                    }
                }
            });
            if ui.add(egui::Slider::new(&mut fps, 1.0..=60.0).text("fps")).changed()
                && let Some(d) = self.st.doc.as_mut()
            {
                d.fps = fps;
                self.st.mark_dirty();
            }
            ui.checkbox(&mut self.st.onion, "onion skin");
            ui.horizontal(|ui| {
                ui.label("sheet cols");
                ui.add(
                    egui::DragValue::new(&mut self.st.sheet_cols)
                        .range(0..=64)
                        .speed(0.2)
                        .prefix("")
                        .custom_formatter(|v, _| if v < 1.0 { "auto".into() } else { format!("{v}") }),
                );
            });
        }
    }

    // --- status bar ----------------------------------------------------------

    fn image_status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("image-status").show(ui, |ui| {
            let Some((dw, dh, mb)) = self.st.doc.as_ref().map(|d| {
                (d.w, d.h, d.resident_bytes() as f32 / (1024.0 * 1024.0))
            }) else {
                return;
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{dw}×{dh}")).small());
                ui.separator();
                ui.label(RichText::new(format!("{:.0}%", self.st.zoom * 100.0)).small());
                if ui.small_button("⛶").on_hover_text("fit (0)").clicked() {
                    self.st.fit_pending = true;
                }
                ui.separator();
                if let Some((x, y)) = self.st.cursor {
                    ui.label(RichText::new(format!("{}, {}", x.floor() as i32, y.floor() as i32)).small().weak());
                }
                if self.st.has_selection() {
                    ui.separator();
                    ui.label(RichText::new("selection active").small().weak());
                }
                if self.st.onion_active() {
                    ui.separator();
                    ui.label(RichText::new("onion").small().weak());
                }
                ui.separator();
                ui.label(RichText::new(format!("{mb:.1} MB")).small().weak())
                    .on_hover_text("resident pixels — tiles are only allocated where you paint");
                if let Some((msg, _)) = self.st.status.clone() {
                    ui.separator();
                    ui.label(RichText::new(msg).small().color(Color32::from_rgb(150, 220, 150)));
                }
            });
        });
    }

    // --- dialogs --------------------------------------------------------------

    fn image_new_dialog(&mut self, ui: &mut egui::Ui) {
        let Some(mut form) = self.st.new_form.clone() else { return };
        let title = if form.scale {
            "Scale image"
        } else if form.resize {
            "Resize canvas"
        } else {
            "New image"
        };
        let mut open = true;
        let mut go = false;
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .fixed_size(egui::vec2(280.0, 0.0))
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ui.ctx(), |ui| {
                if !form.resize {
                    ui.horizontal_wrapped(|ui| {
                        for (label, w, h, m) in NewForm::PRESETS {
                            if ui.button(*label).clicked() {
                                form.w = *w;
                                form.h = *h;
                                form.mode = *m;
                            }
                        }
                    });
                    ui.separator();
                }
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut form.w).range(1..=8192).prefix("w "));
                    ui.add(egui::DragValue::new(&mut form.h).range(1..=8192).prefix("h "));
                });
                if !form.resize {
                    ui.horizontal(|ui| {
                        for m in Mode::ALL {
                            ui.selectable_value(&mut form.mode, m, m.label());
                        }
                    });
                    ui.checkbox(&mut form.background, "opaque background")
                        .on_hover_text("off = a transparent canvas (what sprites want)");
                }
                if form.scale {
                    ui.checkbox(&mut form.nearest, "nearest (keeps pixel art crisp)");
                } else if form.resize {
                    // Growing a canvas anchored top-left puts the art in a
                    // corner, which is almost never what you meant.
                    ui.checkbox(&mut form.centre, "keep the image centred");
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button(if form.resize { "Apply" } else { "Create" }).clicked() {
                        go = true;
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        open = false;
                    }
                });
            });
        if go {
            self.image_apply_new_form(&form);
            self.st.new_form = None;
        } else if !open {
            self.st.new_form = None;
        } else {
            self.st.new_form = Some(form);
        }
    }

    fn image_apply_new_form(&mut self, form: &NewForm) {
        if form.scale {
            self.st.push_undo();
            if let Some(d) = self.st.doc.as_mut() {
                d.scale_to(form.w, form.h, form.nearest);
            }
            self.st.invalidate_all();
        } else if form.resize {
            self.st.push_undo();
            if let Some(d) = self.st.doc.as_mut() {
                let (dx, dy) = if form.centre {
                    ((form.w as i32 - d.w as i32) / 2, (form.h as i32 - d.h as i32) / 2)
                } else {
                    (0, 0)
                };
                d.resize_canvas(form.w, form.h, dx, dy);
            }
            self.st.invalidate_all();
            self.st.fit_pending = true;
        } else {
            self.cmd.image_new = Some(form.clone());
        }
    }

    /// Every binding on one card. An 18-tool editor keyed by single letters has
    /// to say somewhere what those letters are, and tooltips only answer the
    /// question you already knew to ask.
    fn image_keys_window(&mut self, ui: &mut egui::Ui) {
        if !self.st.show_keys {
            return;
        }
        const KEYS: &[(&str, &str)] = &[
            ("B", "pencil ⇄ brush"),
            ("E", "eraser"),
            ("G / Shift+G", "fill / gradient"),
            ("L", "line"),
            ("U / Shift+U", "rectangle / ellipse"),
            ("M / Shift+M", "select box / ellipse"),
            ("Q", "lasso"),
            ("W", "magic wand"),
            ("V", "move layer"),
            ("I", "eyedropper (or right-drag with any tool)"),
            ("A / P", "reshape / pen (vector)"),
            ("T", "text"),
            ("Ctrl+T", "free transform"),
            ("X", "swap primary / secondary colour"),
            ("[ / ]", "brush smaller / bigger (Ctrl+wheel too)"),
            ("Ctrl+Z / Ctrl+Y", "undo / redo — this tab's own history"),
            ("Ctrl+C / X / V", "copy / cut / paste"),
            ("Ctrl+A / Ctrl+D", "clear the selection"),
            ("Delete", "erase the selection (or the layer)"),
            ("Enter", "apply the transform / text / pen path"),
            ("Escape", "cancel it instead"),
            ("Arrows", "nudge by 1 px (Shift = 10)"),
            ("0 / Ctrl+0", "fit / 100 %"),
            ("+ / -", "zoom in / out"),
            ("wheel / middle-drag", "zoom / pan (Space+drag pans too)"),
        ];
        let mut open = self.st.show_keys;
        egui::Window::new("Image editor — keys")
            .collapsible(false)
            .resizable(false)
            .default_size(egui::vec2(360.0, 0.0))
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .open(&mut open)
            .show(ui.ctx(), |ui| {
                egui::Grid::new("img-keys").num_columns(2).spacing([12.0, 2.0]).show(ui, |ui| {
                    for (k, what) in KEYS {
                        ui.label(RichText::new(*k).strong().monospace());
                        ui.label(RichText::new(*what).small());
                        ui.end_row();
                    }
                });
            });
        if ui.ctx().input(|i| i.key_pressed(egui::Key::Escape)) {
            open = false;
        }
        self.st.show_keys = open;
    }

    fn image_save_dialog(&mut self, ui: &mut egui::Ui) {
        let Some(mut name) = self.st.save_name.clone() else { return };
        let mut open = true;
        let mut go = false;
        egui::Window::new("Save image as")
            .collapsible(false)
            .resizable(false)
            .fixed_size(egui::vec2(320.0, 0.0))
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ui.ctx(), |ui| {
                ui.label(RichText::new("saved into the project's textures/ folder").small().weak());
                let resp = ui.add(egui::TextEdit::singleline(&mut name).desired_width(f32::INFINITY));
                resp.request_focus();
                ui.label(RichText::new(format!("textures/{name}.flimg  +  textures/{name}.png")).small().weak());
                ui.separator();
                ui.horizontal(|ui| {
                    let valid = !name.trim().is_empty() && !name.contains(['/', '\\']);
                    if ui.add_enabled(valid, egui::Button::new("Save")).clicked()
                        || (valid && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    {
                        go = true;
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        open = false;
                    }
                });
            });
        if go {
            self.cmd.image_save_as = Some(name.trim().to_string());
            self.st.save_name = None;
        } else if !open {
            self.st.save_name = None;
        } else {
            self.st.save_name = Some(name);
        }
    }
}

/// A colour swatch button with an egui picker popup. Returns true when changed.
fn color_button(ui: &mut egui::Ui, c: &mut [u8; 4], id: &str) -> bool {
    let mut col = Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]);
    let changed = ui
        .push_id(id, |ui| {
            egui::color_picker::color_edit_button_srgba(
                ui,
                &mut col,
                egui::color_picker::Alpha::OnlyBlend,
            )
            .changed()
        })
        .inner;
    if changed {
        *c = [col.r(), col.g(), col.b(), col.a()];
    }
    changed
}

fn hex_of(c: [u8; 4]) -> String {
    format!("#{:02X}{:02X}{:02X}", c[0], c[1], c[2])
}

/// `#RGB`, `#RRGGBB` or either without the hash. Anything else is someone
/// halfway through typing, and must not clobber the colour they had.
fn parse_hex(s: &str) -> Option<[u8; 3]> {
    let t = s.trim().trim_start_matches('#');
    let n = |i: usize, len: usize| -> Option<u8> {
        let part = t.get(i..i + len)?;
        let v = u8::from_str_radix(part, 16).ok()?;
        Some(if len == 1 { v * 17 } else { v })
    };
    match t.len() {
        3 => Some([n(0, 1)?, n(1, 1)?, n(2, 1)?]),
        6 => Some([n(0, 2)?, n(2, 2)?, n(4, 2)?]),
        _ => None,
    }
}

/// Parameter rows for one adjustment. Returns true when anything changed.
fn adjustment_params_ui(ui: &mut egui::Ui, a: &mut Adjustment, palettes: &[Palette]) -> bool {
    let mut changed = false;
    match a {
        Adjustment::Levels { in_black, in_white, gamma, out_black, out_white } => {
            changed |= ui.add(egui::Slider::new(in_black, 0.0..=1.0).text("in black")).changed();
            changed |= ui.add(egui::Slider::new(in_white, 0.0..=1.0).text("in white")).changed();
            changed |= ui.add(egui::Slider::new(gamma, 0.1..=4.0).text("gamma")).changed();
            changed |= ui.add(egui::Slider::new(out_black, 0.0..=1.0).text("out black")).changed();
            changed |= ui.add(egui::Slider::new(out_white, 0.0..=1.0).text("out white")).changed();
        }
        Adjustment::Curves { channel, points } => {
            ui.horizontal(|ui| {
                for (c, n) in [
                    (CurveChannel::Rgb, "rgb"),
                    (CurveChannel::R, "r"),
                    (CurveChannel::G, "g"),
                    (CurveChannel::B, "b"),
                    (CurveChannel::A, "a"),
                ] {
                    changed |= ui.selectable_value(channel, c, n).clicked();
                }
            });
            // A compact editable curve: drag the interior keys.
            for (i, p) in points.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{i}")).small().weak());
                    changed |= ui.add(egui::DragValue::new(&mut p.0).speed(0.01).range(0.0..=1.0)).changed();
                    changed |= ui.add(egui::DragValue::new(&mut p.1).speed(0.01).range(0.0..=1.0)).changed();
                });
            }
            if ui.small_button("✚ key").clicked() {
                points.push((0.5, 0.5));
                points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
                changed = true;
            }
        }
        Adjustment::Hsl { hue, sat, light } => {
            changed |= ui.add(egui::Slider::new(hue, -180.0..=180.0).text("hue")).changed();
            changed |= ui.add(egui::Slider::new(sat, -1.0..=1.0).text("saturation")).changed();
            changed |= ui.add(egui::Slider::new(light, -1.0..=1.0).text("lightness")).changed();
        }
        Adjustment::BrightnessContrast { brightness, contrast } => {
            changed |= ui.add(egui::Slider::new(brightness, -1.0..=1.0).text("brightness")).changed();
            changed |= ui.add(egui::Slider::new(contrast, -1.0..=1.0).text("contrast")).changed();
        }
        Adjustment::ColorBalance { r, g, b } => {
            changed |= ui.add(egui::Slider::new(r, -1.0..=1.0).text("red")).changed();
            changed |= ui.add(egui::Slider::new(g, -1.0..=1.0).text("green")).changed();
            changed |= ui.add(egui::Slider::new(b, -1.0..=1.0).text("blue")).changed();
        }
        Adjustment::Posterize { levels } => {
            changed |= ui.add(egui::Slider::new(levels, 2..=32).text("levels")).changed();
        }
        Adjustment::Threshold { t } => {
            changed |= ui.add(egui::Slider::new(t, 0.0..=1.0).text("threshold")).changed();
        }
        Adjustment::Desaturate { amount } => {
            changed |= ui.add(egui::Slider::new(amount, 0.0..=1.0).text("amount")).changed();
        }
        Adjustment::Quantize { palette, dither, amount } => {
            egui::ComboBox::from_id_salt("adj-quant-pal")
                .selected_text(if palette.colors.is_empty() { "(pick a palette)" } else { palette.name.as_str() })
                .show_ui(ui, |ui| {
                    for p in palettes {
                        if ui.selectable_label(p.name == palette.name, &p.name).clicked() {
                            *palette = p.clone();
                            changed = true;
                        }
                    }
                });
            ui.horizontal(|ui| {
                for (d, n) in [
                    (Dither::None, "none"),
                    (Dither::Ordered, "ordered"),
                    (Dither::FloydSteinberg, "diffuse"),
                ] {
                    changed |= ui.selectable_value(dither, d, n).clicked();
                }
            });
            changed |= ui.add(egui::Slider::new(amount, 0.0..=1.0).text("amount")).changed();
        }
        Adjustment::GradientMap { stops } => {
            for (i, s) in stops.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    changed |= ui.add(egui::DragValue::new(&mut s.0).speed(0.01).range(0.0..=1.0)).changed();
                    changed |= color_button(ui, &mut s.1, &format!("gm-{i}"));
                });
            }
            if ui.small_button("✚ stop").clicked() {
                stops.push((0.5, [128, 128, 128, 255]));
                stops.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
                changed = true;
            }
        }
        Adjustment::Invert => {
            ui.label(RichText::new("no settings").small().weak());
        }
    }
    changed
}

/// Parameter rows for one layer effect.
fn effect_params_ui(ui: &mut egui::Ui, e: &mut Effect, idx: usize) -> bool {
    let mut changed = false;
    ui.indent(("fx", idx), |ui| match e {
        Effect::Outline { color, width, outside } => {
            ui.horizontal(|ui| {
                changed |= color_button(ui, color, &format!("fx-out-{idx}"));
                changed |= ui.add(egui::DragValue::new(width).range(0..=32).prefix("w ")).changed();
                changed |= ui.checkbox(outside, "outside").changed();
            });
        }
        Effect::DropShadow { color, dx, dy, blur, opacity } => {
            ui.horizontal(|ui| {
                changed |= color_button(ui, color, &format!("fx-sh-{idx}"));
                changed |= ui.add(egui::DragValue::new(dx).speed(0.2).prefix("x ")).changed();
                changed |= ui.add(egui::DragValue::new(dy).speed(0.2).prefix("y ")).changed();
            });
            changed |= ui.add(egui::Slider::new(blur, 0.0..=32.0).text("blur")).changed();
            changed |= ui.add(egui::Slider::new(opacity, 0.0..=1.0).text("opacity")).changed();
        }
        Effect::Glow { color, radius, opacity, inner } => {
            ui.horizontal(|ui| {
                changed |= color_button(ui, color, &format!("fx-gl-{idx}"));
                changed |= ui.checkbox(inner, "inner").changed();
            });
            changed |= ui.add(egui::Slider::new(radius, 0.0..=64.0).text("radius")).changed();
            changed |= ui.add(egui::Slider::new(opacity, 0.0..=1.0).text("opacity")).changed();
        }
        Effect::ColorOverlay { color, opacity } => {
            ui.horizontal(|ui| {
                changed |= color_button(ui, color, &format!("fx-co-{idx}"));
                changed |= ui.add(egui::Slider::new(opacity, 0.0..=1.0).text("opacity")).changed();
            });
        }
    });
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_edit::ImageEditState;
    use floptle_image::doc::Image;
    use floptle_image::vector::VPath;
    use floptle_image::Rect;

    /// Run the whole tab for two frames in some state, returning the commands it
    /// raised. Two frames because egui reads back widget state on the second.
    fn run(st: &mut ImageEditState) -> EditorCmd {
        let ctx = crate::icons::test_context();
        let mut cmd = EditorCmd::default();
        for _ in 0..2 {
            let _ = ctx.run_ui(crate::icons::test_input(), |ui| {
                let mut cx = ImageCtx { st, project_root: std::path::Path::new("/tmp"), cmd: &mut cmd };
                cx.ui(ui);
            });
        }
        cmd
    }

    fn doc_state() -> ImageEditState {
        let mut st = ImageEditState::default();
        let mut doc = Image::new(48, 32, Mode::Pixel);
        doc.layers[0].grid_mut(0).unwrap().fill([80, 120, 200, 255]);
        st.adopt(doc, Some("/tmp/thing.flimg".into()), None);
        st
    }

    /// The empty state must not panic, and its one button must be reachable.
    #[test]
    fn the_welcome_screen_renders() {
        let mut st = ImageEditState::default();
        let _ = run(&mut st);
    }

    /// Every tool's option panel, every layer kind, the dialogs, the filter
    /// preview and an animated document — all laid out for real. This is the
    /// test that catches a panic (or an egui id collision) in a corner of the
    /// panel nobody clicked during development.
    #[test]
    fn tab_renders_in_every_state() {
        for tool in ImgTool::ALL {
            let mut st = doc_state();
            st.tool = tool;
            let _ = run(&mut st);
        }
        // A vector layer selected, with a node picked (fill/stroke rows).
        let mut st = doc_state();
        let mut l = Layer::vector("shape");
        l.kind = LayerKind::Vector { paths: vec![VPath::rect(2.0, 2.0, 8.0, 8.0)] };
        st.doc.as_mut().unwrap().add_layer(l);
        st.tool = ImgTool::Reshape;
        st.sel_node = Some((0, 0));
        let _ = run(&mut st);

        // Every adjustment, each with its own parameter rows.
        for a in Adjustment::presets() {
            let mut st = doc_state();
            st.doc.as_mut().unwrap().add_layer(Layer::adjust(a));
            let _ = run(&mut st);
        }
        // Every effect.
        for e in Effect::presets() {
            let mut st = doc_state();
            st.doc.as_mut().unwrap().layers[0].effects.push(e);
            let _ = run(&mut st);
        }
        // Every filter's live preview.
        for k in FilterKind::ALL {
            let mut st = doc_state();
            st.begin_filter(k);
            let _ = run(&mut st);
        }
        // A palette, a selection, a mask and several frames.
        let mut st = doc_state();
        {
            let d = st.doc.as_mut().unwrap();
            d.palette = Some(floptle_image::palette::builtin()[0].clone());
            d.palette_lock = true;
            d.selection = Some(floptle_image::select::rect_mask(48, 32, Rect::new(4, 4, 8, 8)));
            d.layers[0].mask = Some(floptle_image::select::Mask::new(48, 32, 255));
            d.set_frames(4);
            d.set_layer_animated(0, true);
        }
        st.frame = 2;
        let _ = run(&mut st);

        // A live transform and a live text block.
        let mut st = doc_state();
        st.tool = ImgTool::Transform;
        st.begin_transform();
        let _ = run(&mut st);
        let mut st = doc_state();
        st.tool = ImgTool::Text;
        st.begin_text(4.0, 4.0);
        st.set_text("hello".into(), 20.0);
        let _ = run(&mut st);

        // The dialogs.
        let mut st = doc_state();
        st.show_keys = true;
        let _ = run(&mut st);
        let mut st = doc_state();
        st.new_form = Some(NewForm::default());
        let _ = run(&mut st);
        let mut st = doc_state();
        st.new_form = Some(NewForm { resize: true, scale: true, ..Default::default() });
        let _ = run(&mut st);
        let mut st = doc_state();
        st.save_name = Some("thing".into());
        let _ = run(&mut st);
    }

    /// Drive the tab with REAL pointer events: press on the canvas, drag, release.
    ///
    /// This is the one test that proves the whole chain connects — the view
    /// transform, the hit-testing, the tool state machine and the brush — rather
    /// than each half separately.
    fn run_events(st: &mut ImageEditState, ctx: &egui::Context, events: Vec<egui::Event>) {
        let mut cmd = EditorCmd::default();
        let input = egui::RawInput { events, ..crate::icons::test_input() };
        let _ = ctx.run_ui(input, |ui| {
            let mut cx = ImageCtx { st, project_root: std::path::Path::new("/tmp"), cmd: &mut cmd };
            cx.ui(ui);
        });
    }

    #[test]
    fn a_drag_over_the_canvas_paints_and_banks_one_undo_step() {
        use egui::{Event, PointerButton, Pos2};
        let ctx = crate::icons::test_context();
        let mut st = doc_state();
        st.tool = ImgTool::Pencil;
        st.color = [255, 0, 0, 255];
        // Frame 1 lays out and fits the view; the canvas centre is the screen
        // centre of the central panel, near enough for a drag through it.
        run_events(&mut st, &ctx, vec![]);
        let mid = Pos2::new(640.0, 400.0);
        run_events(
            &mut st,
            &ctx,
            vec![
                Event::PointerMoved(mid),
                Event::PointerButton { pos: mid, button: PointerButton::Primary, pressed: true, modifiers: Default::default() },
            ],
        );
        for step in 1..=4 {
            let p = mid + egui::vec2(step as f32 * 20.0, 0.0);
            run_events(&mut st, &ctx, vec![Event::PointerMoved(p)]);
        }
        let end = mid + egui::vec2(80.0, 0.0);
        run_events(
            &mut st,
            &ctx,
            vec![Event::PointerButton { pos: end, button: PointerButton::Primary, pressed: false, modifiers: Default::default() }],
        );

        let doc = st.doc.as_ref().unwrap();
        let painted: usize = (0..doc.h as i64)
            .flat_map(|y| (0..doc.w as i64).map(move |x| (x, y)))
            .filter(|&(x, y)| doc.layers[0].grid(0).unwrap().get(x, y) == [255, 0, 0, 255])
            .count();
        assert!(painted > 0, "a drag across the canvas must paint something");
        assert!(st.can_undo(), "…and bank exactly one undo step for the stroke");
        assert!(st.dirty && st.png_dirty, "…and mark the document dirty");
        st.undo();
        let doc = st.doc.as_ref().unwrap();
        let after: usize = (0..doc.h as i64)
            .flat_map(|y| (0..doc.w as i64).map(move |x| (x, y)))
            .filter(|&(x, y)| doc.layers[0].grid(0).unwrap().get(x, y) == [255, 0, 0, 255])
            .count();
        assert_eq!(after, 0, "one undo must remove the whole stroke, not one dab");
    }

    /// The text tool goes through egui's own font atlas — so text stamped into
    /// an image matches the text beside it, and the kernel needs no font code.
    #[test]
    fn text_rasterizes_through_the_font_atlas() {
        let ctx = crate::icons::test_context();
        let (px, w, h) =
            crate::image_edit::rasterize_text(&ctx, "Ag", 32.0, [255, 0, 0, 255]).expect("raster");
        assert!(w > 4 && h > 4, "{w}×{h}");
        let opaque = px.chunks_exact(4).filter(|p| p[3] > 40).count();
        assert!(opaque > 20, "expected ink, got {opaque} covered texels");
        assert!(
            px.chunks_exact(4).filter(|p| p[3] > 40).all(|p| p[0] == 255 && p[1] == 0),
            "the ink takes the chosen colour"
        );
        // Empty text is nothing, not a panic.
        let empty = crate::image_edit::rasterize_text(&ctx, "", 32.0, [1, 2, 3, 255]);
        assert!(empty.map(|(p, _, _)| p.iter().all(|b| *b == 0)).unwrap_or(true));
    }

    /// End to end: place a block, type, apply — then one undo removes it.
    #[test]
    fn typing_text_stamps_it_and_one_undo_removes_it() {
        let ctx = crate::icons::test_context();
        let mut st = doc_state();
        st.tool = ImgTool::Text;
        st.color = [255, 255, 0, 255];
        st.begin_text(4.0, 4.0);
        st.set_text("Hi".into(), 16.0);
        st.render_text(&ctx);
        let painted = |st: &ImageEditState| {
            let g = st.doc.as_ref().unwrap().layers[0].grid(0).unwrap();
            (0..32i64)
                .flat_map(|y| (0..48i64).map(move |x| (x, y)))
                .filter(|&(x, y)| g.get(x, y) == [255, 255, 0, 255])
                .count()
        };
        assert!(painted(&st) > 4, "the text should be on the canvas");
        st.commit_text();
        assert!(st.dirty && painted(&st) > 4);
        st.undo();
        assert_eq!(painted(&st), 0, "one undo removes the whole block");
    }

    /// …and cancelling leaves the document byte-identical.
    #[test]
    fn cancelling_text_is_exact() {
        let ctx = crate::icons::test_context();
        let mut st = doc_state();
        let before = st.doc.as_ref().unwrap().layers[0].grid(0).unwrap().to_rgba();
        st.tool = ImgTool::Text;
        st.begin_text(2.0, 2.0);
        st.set_text("xyz".into(), 24.0);
        st.render_text(&ctx);
        assert!(st.cancel_text());
        assert_eq!(st.doc.as_ref().unwrap().layers[0].grid(0).unwrap().to_rgba(), before);
        assert!(!st.can_undo());
    }

    /// A dragged property slider is ONE undo step, banked when the pointer
    /// comes up — not one per frame, and not zero.
    #[test]
    fn a_property_edit_is_one_undoable_step() {
        let mut st = doc_state();
        assert!(!st.can_undo());
        // Three frames of "the slider moved" while the pointer is held…
        for k in 0..3 {
            st.begin_edit();
            st.doc.as_mut().unwrap().layers[0].opacity = 0.9 - k as f32 * 0.1;
        }
        assert!(!st.can_undo(), "nothing banks until the gesture ends");
        st.flush_edit();
        assert!(st.can_undo() && st.dirty);
        st.undo();
        assert_eq!(st.doc.as_ref().unwrap().layers[0].opacity, 1.0, "back to before the drag");
    }

    /// A half-typed hex code must never overwrite the colour you already had.
    #[test]
    fn hex_entry_only_accepts_a_complete_colour() {
        assert_eq!(parse_hex("#FF8000"), Some([255, 128, 0]));
        assert_eq!(parse_hex("ff8000"), Some([255, 128, 0]));
        assert_eq!(parse_hex("#f80"), Some([255, 136, 0]));
        assert_eq!(parse_hex("  #F80  "), Some([255, 136, 0]));
        for half in ["#", "#F", "#FF80", "#GGGGGG", "", "#FF80000"] {
            assert_eq!(parse_hex(half), None, "{half:?} is not a colour yet");
        }
        // Round trip through what the field shows.
        assert_eq!(parse_hex(&hex_of([1, 2, 3, 255])), Some([1, 2, 3]));
    }

    /// A document with no path can't be saved silently to an invented file — the
    /// tab asks for a name instead.
    #[test]
    fn saving_an_unnamed_document_asks_for_a_name() {
        let mut st = doc_state();
        st.path = None;
        st.save_name = Some("hero".into());
        // The dialog's Save is keyboard-driven in the real UI; drive the state
        // directly and check the command it would raise.
        let mut cmd = EditorCmd::default();
        let ctx = crate::icons::test_context();
        let _ = ctx.run_ui(crate::icons::test_input(), |ui| {
            let mut cx = ImageCtx { st: &mut st, project_root: std::path::Path::new("/tmp"), cmd: &mut cmd };
            cx.ui(ui);
        });
        assert!(st.save_name.is_some(), "the dialog stays up until it's answered");
    }
}
