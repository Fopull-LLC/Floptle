//! The Assets dock tab: the folder grid browser (tiles, drag sources, create/
//! rename/delete menus), the tree fallback, the Inspector-side asset preview,
//! and the per-texture / material-preset asset editors.

use std::path::{Path, PathBuf};

use floptle_scene::MaterialDoc;

use crate::assets::{
    asset_kind_icon, asset_rel_path, is_markdown, is_prefab, is_scene, is_script,
    reveal_in_explorer, truncate_label, AssetEntry, AssetPayload, FilterMode, WrapMode,
};
use crate::hierarchy::NodePayload;
use crate::inspector::material_props_ui;
use crate::{anim, anim_ui, EditorTabViewer, PreviewView};

impl<'a> EditorTabViewer<'a> {
    pub(crate) fn assets_ui(&mut self, ui: &mut egui::Ui) {
        let root = self.project_root.to_path_buf();
        ui.horizontal(|ui| {
            ui.strong("Assets");
            if ui.small_button("⟳").on_hover_text("rescan").clicked() {
                self.cmd.refresh_assets = true;
            }
            ui.menu_button("✚ New", |ui| {
                self.new_asset_menu(ui, &root);
            });
            ui.separator();
            // Tree / Grid view toggle.
            if ui.selectable_label(!*self.assets_grid, "☰").on_hover_text("file tree").clicked() {
                *self.assets_grid = false;
            }
            if ui.selectable_label(*self.assets_grid, "⊞").on_hover_text("icon grid").clicked() {
                *self.assets_grid = true;
            }
            ui.separator();
            ui.small("Ctrl/Shift-click to multi-select · drag onto a folder to move, onto the scene to spawn, onto a picker to assign · drag a node HERE to make a prefab · right-click for New");
        });
        ui.separator();
        if *self.assets_grid {
            self.assets_grid_ui(ui, &root);
            return;
        }
        let tree = self.asset_tree; // Copy the slice ref so the recursion can &mut self.
        let resp = egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.asset_node_ui(ui, tree, &root);
                // Catch right-clicks on the empty space below the list so New
                // Folder / New Script is reachable even when the tree is short.
                ui.allocate_response(ui.available_size(), egui::Sense::click())
            })
            .inner;
        // Drop a Hierarchy node on the empty space → save it as a prefab
        // (lands in the canonical prefabs/ folder; drop on a folder to aim).
        self.node_drop_makes_prefab(ui, &resp, &root.join("prefabs"));
        resp.context_menu(|ui| {
            self.new_asset_menu(ui, &root);
        });
    }

    /// Accept a dragged Hierarchy node on `resp`: highlight while hovered and
    /// save the node (whole subtree; the multi-selection if it's part of one)
    /// as a prefab file in `dir` on release.
    fn node_drop_makes_prefab(&mut self, ui: &egui::Ui, resp: &egui::Response, dir: &Path) {
        if resp.dnd_hover_payload::<NodePayload>().is_some() {
            ui.painter().rect_stroke(
                resp.rect.shrink(2.0),
                5.0,
                egui::Stroke::new(2.0, ui.visuals().selection.stroke.color),
                egui::StrokeKind::Inside,
            );
            if let Some(pos) = ui.ctx().pointer_hover_pos() {
                egui::Area::new(egui::Id::new("prefab-drop-tip"))
                    .fixed_pos(pos + egui::vec2(14.0, 14.0))
                    .order(egui::Order::Tooltip)
                    .show(ui.ctx(), |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.small("⬡ save as prefab");
                        });
                    });
            }
        }
        if let Some(p) = resp.dnd_release_payload::<NodePayload>() {
            let roots = if self.selection.contains(&p.0) && self.selection.len() > 1 {
                self.selection.clone()
            } else {
                vec![p.0]
            };
            self.cmd.save_prefab = Some((roots, dir.to_path_buf()));
        }
    }

    /// Handle a click on file `path` in the browser: plain = single-select,
    /// Ctrl/Cmd = toggle, Shift = range within `order` (the current view's flat
    /// file list). Keeps `selected_asset` as the primary (drives the preview).
    fn asset_click(&mut self, ui: &egui::Ui, path: &str, order: &[String]) {
        let m = ui.input(|i| i.modifiers);
        if m.command || m.ctrl {
            if let Some(i) = self.asset_selection.iter().position(|p| p == path) {
                self.asset_selection.remove(i);
            } else {
                self.asset_selection.push(path.to_string());
            }
        } else if m.shift
            && let Some(anchor) = self.selected_asset.clone()
            && let (Some(a), Some(b)) =
                (order.iter().position(|p| *p == anchor), order.iter().position(|p| p == path))
        {
            let (lo, hi) = (a.min(b), a.max(b));
            *self.asset_selection = order[lo..=hi].to_vec();
        } else {
            *self.asset_selection = vec![path.to_string()];
        }
        *self.selected_asset = Some(path.to_string());
    }

    /// Whether `path` is part of the current browser selection.
    fn asset_is_selected(&self, path: &str) -> bool {
        self.selected_asset.as_deref() == Some(path)
            || self.asset_selection.iter().any(|p| p == path)
    }

    /// The set of paths a drag starting on `dragged` should move: the whole
    /// multi-selection when the dragged item is part of it, else just itself.
    fn move_sources(&self, dragged: &str) -> Vec<String> {
        if self.asset_selection.iter().any(|p| p == dragged) && self.asset_selection.len() > 1 {
            self.asset_selection.clone()
        } else {
            vec![dragged.to_string()]
        }
    }

    /// Find the asset entries inside `dir` (absolute, under the project root) by
    /// walking the cached tree. The returned slice borrows the tree (lifetime `'a`),
    /// not `self`, so the caller can still `&mut self` while iterating it.
    pub(crate) fn grid_entries(&self, dir: &Path) -> Option<&'a [AssetEntry]> {
        let rel = dir.strip_prefix(self.project_root).ok()?;
        let mut cur: &'a [AssetEntry] = self.asset_tree;
        for comp in rel.components() {
            let name = comp.as_os_str().to_string_lossy();
            cur = cur.iter().find_map(|e| match e {
                AssetEntry::Dir(n, kids) if n.as_str() == name => Some(kids.as_slice()),
                _ => None,
            })?;
        }
        Some(cur)
    }

    /// The icon-grid asset browser: a wrapped flow of tiles for the current folder.
    /// Folders descend on double-click; files select / open / drag like the tree.
    pub(crate) fn assets_grid_ui(&mut self, ui: &mut egui::Ui, root: &Path) {
        // Keep the grid folder valid (e.g. after switching projects).
        if !self.assets_grid_dir.starts_with(root) {
            *self.assets_grid_dir = root.to_path_buf();
        }
        let dir = self.assets_grid_dir.clone();

        // Breadcrumb row: up button + relative path.
        ui.horizontal(|ui| {
            let at_root = dir == root;
            if ui.add_enabled(!at_root, egui::Button::new("⏶")).on_hover_text("up").clicked()
                && let Some(p) = dir.parent() {
                    *self.assets_grid_dir = p.to_path_buf();
                }
            let rel = dir.strip_prefix(root).ok().map(|p| p.to_string_lossy().to_string());
            let crumb = match rel.as_deref() {
                Some("") | None => "assets".to_string(),
                Some(r) => format!("assets/{r}"),
            };
            ui.weak(crumb);
        });
        ui.separator();

        let Some(entries) = self.grid_entries(&dir) else {
            ui.weak("(empty)");
            return;
        };
        // Ordered file list of this folder — the range for Shift-select.
        let order: Vec<String> = entries
            .iter()
            .filter_map(|e| match e {
                AssetEntry::File { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect();
        let mut enter: Option<PathBuf> = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                for entry in entries {
                    match entry {
                        AssetEntry::Dir(name, _) => {
                            let folder = dir.join(name);
                            let resp = self.folder_tile(ui, name.as_str(), &folder);
                            if resp.double_clicked() {
                                enter = Some(folder);
                            }
                        }
                        AssetEntry::File { name, path } => {
                            let (icon, color) = asset_kind_icon(path.as_str());
                            self.asset_file_tile(ui, icon, color, name.as_str(), path.as_str(), &order);
                        }
                    }
                }
            });
            // Right-click empty space ⏵ New menu; drop a Hierarchy node here
            // ⏵ save it as a prefab in the folder you're looking at.
            let bg = ui.allocate_response(ui.available_size(), egui::Sense::click());
            self.node_drop_makes_prefab(ui, &bg, &dir);
            bg.context_menu(|ui| self.new_asset_menu(ui, &dir));
        });
        if let Some(d) = enter {
            *self.assets_grid_dir = d;
        }
    }

    /// A folder tile: double-click to descend, and a DROP TARGET — release a
    /// dragged asset (or the whole selection) on it to move the files inside,
    /// or a Hierarchy node to save it as a prefab here.
    pub(crate) fn folder_tile(&mut self, ui: &mut egui::Ui, name: &str, dir: &Path) -> egui::Response {
        let resp = self.tile_frame(ui, "🗀", egui::Color32::from_rgb(225, 200, 130), name, false);
        if resp.dnd_hover_payload::<AssetPayload>().is_some() {
            ui.painter().rect_stroke(
                resp.rect.shrink(2.0),
                5.0,
                egui::Stroke::new(2.0, ui.visuals().selection.stroke.color),
                egui::StrokeKind::Inside,
            );
        }
        if let Some(p) = resp.dnd_release_payload::<AssetPayload>() {
            self.cmd.move_assets = Some((self.move_sources(&p.path), dir.to_path_buf()));
        }
        self.node_drop_makes_prefab(ui, &resp, dir);
        resp.context_menu(|ui| self.folder_menu(ui, dir));
        resp
    }

    /// The shared folder context menu (tree header + grid tile): New…, then
    /// Rename / Reveal / Delete — the same verbs files get.
    fn folder_menu(&mut self, ui: &mut egui::Ui, dir: &Path) {
        self.new_asset_menu(ui, dir);
        ui.separator();
        if ui.button("🖊 Rename…").clicked() {
            self.cmd.rename_asset = Some(dir.to_string_lossy().to_string());
            ui.close();
        }
        if ui.button("🗀 Open in file explorer").clicked() {
            reveal_in_explorer(dir);
            ui.close();
        }
        if ui.button("🗑 Delete folder").clicked() {
            self.cmd.delete_asset = Some(vec![dir.to_string_lossy().to_string()]);
            ui.close();
        }
    }

    /// A file tile: select on click (Ctrl/Shift multi-select via `order`), open on
    /// double-click (scripts/markdown), drag a payload, and the shared context menu.
    pub(crate) fn asset_file_tile(&mut self, ui: &mut egui::Ui, icon: &str, color: egui::Color32, name: &str, path: &str, order: &[String]) {
        let selected = self.asset_is_selected(path);
        let resp = self.tile_frame(ui, icon, color, name, selected);
        // Every asset is a drag source — drop a model/script/prefab on the scene,
        // or any asset (texture, audio, clip…) onto a matching Inspector picker.
        resp.dnd_set_drag_payload(AssetPayload { path: path.to_string() });
        if resp.clicked() {
            self.asset_click(ui, path, order);
        }
        if resp.double_clicked() {
            self.asset_open(path);
        }
        let dir = Path::new(path).parent().map(|p| p.to_path_buf());
        resp.context_menu(|ui| self.asset_file_menu(ui, path, dir.as_deref()));
    }

    /// Double-click open dispatch for a file, shared by the tree row and grid
    /// tile: scenes open, editors (anim graph / particles) focus, audio
    /// previews, scripts/markdown open in the IDE. Other kinds just select.
    fn asset_open(&mut self, path: &str) {
        if is_scene(path) {
            self.cmd.open_scene = Some(path.to_string());
        } else if anim_ui::is_anim_ctl(path) {
            self.cmd.open_anim_graph = Some(anim::asset_key(
                Path::new(path),
                self.project_root,
                floptle_scene::ANIM_CTL_EXT,
            ));
        } else if crate::assets::is_vfx(path) {
            self.cmd.open_particle_editor = Some(anim::asset_key(
                Path::new(path),
                self.project_root,
                floptle_scene::VFX_EXT,
            ));
        } else if crate::assets::is_audio(path) {
            self.cmd.preview_audio = Some(path.to_string());
        } else if crate::assets::is_shader(path) {
            // Shaders open in the graph by default — the beginner front door;
            // the tab's `</>` button (and the context menu) reach the text.
            self.cmd.open_shader_graph = Some(path.to_string());
        } else if is_script(path) || is_markdown(path) {
            self.cmd.open_script_pref = Some(path.to_string());
        }
    }

    /// The shared file context menu (tree row + grid tile).
    fn asset_file_menu(&mut self, ui: &mut egui::Ui, path: &str, dir: Option<&Path>) {
        if is_prefab(path) {
            if ui
                .button("⬡ Add to scene")
                .on_hover_text("place an instance in front of the camera — or just drag the prefab into the viewport")
                .clicked()
            {
                self.cmd.instantiate_prefab = Some((path.to_string(), None));
                ui.close();
            }
            ui.separator();
        }
        if crate::assets::is_shader(path) && ui.button("◈ Open in Shader Graph").clicked() {
            self.cmd.open_shader_graph = Some(path.to_string());
            ui.close();
        }
        let openable = is_script(path) || is_markdown(path) || crate::assets::is_shader(path);
        if openable && ui.button("🖊 Open in Scripting tab").clicked() {
            self.cmd.open_script = Some(path.to_string());
            self.cmd.focus_scripting = true;
            ui.close();
        }
        if ui.button("🗀 Open in file explorer").clicked() {
            reveal_in_explorer(Path::new(path));
            ui.close();
        }
        if ui.button("⎘ Copy asset path").on_hover_text("the path after Assets/ — paste into assets.getFile(\"…\")").clicked() {
            ui.ctx().copy_text(asset_rel_path(path, self.project_root));
            ui.close();
        }
        if ui.button("⎘ Copy full path").clicked() {
            ui.ctx().copy_text(path.to_string());
            ui.close();
        }
        if ui.button("🖊 Rename…").clicked() {
            self.cmd.rename_asset = Some(path.to_string());
            ui.close();
        }
        // Deleting a file that's part of the multi-selection deletes the whole
        // selection (after the confirm) — the menu says how many up front.
        let targets = self.move_sources(path);
        let del_label = if targets.len() > 1 {
            format!("🗑 Delete {} files", targets.len())
        } else {
            "🗑 Delete".to_string()
        };
        if ui.button(del_label).clicked() {
            self.cmd.delete_asset = Some(targets);
            ui.close();
        }
        if let Some(d) = dir {
            ui.separator();
            self.new_asset_menu(ui, d);
        }
    }

    /// Paint one tile (a framed icon over a name), returning its click_and_drag
    /// response. Highlights when `selected`.
    pub(crate) fn tile_frame(
        &self,
        ui: &mut egui::Ui,
        icon: &str,
        color: egui::Color32,
        name: &str,
        selected: bool,
    ) -> egui::Response {
        let size = egui::vec2(86.0, 84.0);
        let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click_and_drag());
        let p = ui.painter_at(rect);
        let bg = if selected {
            ui.visuals().selection.bg_fill.gamma_multiply(0.5)
        } else if resp.hovered() {
            ui.visuals().widgets.hovered.bg_fill
        } else {
            ui.visuals().faint_bg_color
        };
        p.rect_filled(rect.shrink(2.0), 5.0, bg);
        if selected {
            p.rect_stroke(rect.shrink(2.0), 5.0, egui::Stroke::new(1.5, ui.visuals().selection.stroke.color), egui::StrokeKind::Inside);
        }
        // Icon glyph centered in the upper part.
        let icon_pos = egui::pos2(rect.center().x, rect.top() + 30.0);
        p.text(icon_pos, egui::Align2::CENTER_CENTER, icon, egui::FontId::proportional(30.0), color);
        // Name, truncated to two-ish lines at the bottom.
        let short = truncate_label(name, 22);
        p.text(
            egui::pos2(rect.center().x, rect.bottom() - 16.0),
            egui::Align2::CENTER_CENTER,
            short,
            egui::FontId::proportional(11.0),
            ui.visuals().text_color(),
        );
        resp.on_hover_text(name)
    }

    /// The shared "New Folder / New Script" submenu, targeting `dir`.
    pub(crate) fn new_asset_menu(&mut self, ui: &mut egui::Ui, dir: &Path) {
        if ui.button("🗀 New Folder").clicked() {
            self.cmd.new_folder_in = Some(dir.to_string_lossy().to_string());
            ui.close();
        }
        if ui.button("🖊 New Lua Script").clicked() {
            self.cmd.new_script_in = Some(dir.to_string_lossy().to_string());
            ui.close();
        }
        if ui.button("◈ New Shader").clicked() {
            self.cmd.new_shader_in = Some(dir.to_string_lossy().to_string());
            ui.close();
        }
        if ui.button("⎙ New Scene").clicked() {
            self.cmd.open_new_scene = true;
            ui.close();
        }
        if ui.button("◎ New Animation Controller").clicked() {
            self.cmd.new_anim_controller = Some(None);
            self.cmd.new_anim_controller_dir = Some(dir.to_string_lossy().to_string());
            ui.close();
        }
    }

    pub(crate) fn asset_node_ui(&mut self, ui: &mut egui::Ui, entries: &[AssetEntry], dir: &Path) {
        // This level's file order — the range for Shift-select.
        let order: Vec<String> = entries
            .iter()
            .filter_map(|e| match e {
                AssetEntry::File { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect();
        for entry in entries {
            match entry {
                AssetEntry::Dir(name, children) => {
                    let child_dir = dir.join(name);
                    let header = egui::CollapsingHeader::new(format!("🗀 {name}"))
                        .id_salt(name)
                        .show(ui, |ui| {
                            self.asset_node_ui(ui, children, &child_dir);
                        });
                    // Drop a dragged asset (or the selection) here to move it in,
                    // or a Hierarchy node to save it as a prefab in this folder.
                    let hr = header.header_response.clone();
                    if hr.dnd_hover_payload::<AssetPayload>().is_some() {
                        ui.painter().rect_stroke(
                            hr.rect,
                            2.0,
                            egui::Stroke::new(1.5, ui.visuals().selection.stroke.color),
                            egui::StrokeKind::Outside,
                        );
                    }
                    if let Some(p) = hr.dnd_release_payload::<AssetPayload>() {
                        self.cmd.move_assets =
                            Some((self.move_sources(&p.path), child_dir.clone()));
                    }
                    self.node_drop_makes_prefab(ui, &hr, &child_dir);
                    hr.context_menu(|ui| self.folder_menu(ui, &child_dir));
                }
                AssetEntry::File { name, path } => {
                    // Every asset drags (scene spawn for models/scripts/prefabs;
                    // picker-fill for textures/audio/clips/…).
                    let selected = self.asset_is_selected(path);
                    let (icon, _) = asset_kind_icon(path);
                    let grip = "¦";
                    let label = format!("{grip} {icon} {name}");
                    // A single widget that senses BOTH click and drag. (The old
                    // dnd_drag_source layered a drag-sense interaction over the label,
                    // and the drag sense swallowed double-clicks — so a script could
                    // only be dragged, never opened.) One click_and_drag widget lets
                    // egui tell a tap from a drag cleanly: tap ⏵ select / double-tap
                    // ⏵ open; press-and-move ⏵ drag a payload onto the scene or a node.
                    let text = if selected {
                        egui::RichText::new(label).strong().color(ui.visuals().selection.stroke.color)
                    } else {
                        egui::RichText::new(label)
                    };
                    let resp = ui.add(
                        egui::Label::new(text)
                            .selectable(false)
                            .sense(egui::Sense::click_and_drag()),
                    );
                    resp.dnd_set_drag_payload(AssetPayload { path: path.clone() });
                    if resp.clicked() {
                        self.asset_click(ui, path, &order);
                    }
                    if resp.double_clicked() {
                        self.asset_open(&path.clone());
                    }
                    let path = path.clone();
                    resp.context_menu(|ui| self.asset_file_menu(ui, &path, Some(dir)));
                }
            }
        }
    }

    /// Draw the selected asset's preview: a spinning model/material render (drag to
    /// orbit, scroll to zoom, with spin + zoom controls) or a texture image.
    pub(crate) fn asset_preview_ui(&mut self, ui: &mut egui::Ui) {
        match self.preview.clone() {
            Some(PreviewView::Rendered(id)) => {
                let size = egui::vec2(240.0, 240.0);
                let resp = ui.add(
                    egui::Image::new((id, size))
                        .sense(egui::Sense::click_and_drag())
                        .corner_radius(4.0),
                );
                // Drag to orbit (pauses auto-spin); scroll over the image to zoom.
                if resp.dragged() {
                    *self.preview_spinning = false;
                    *self.preview_spin += resp.drag_delta().x * 0.01;
                }
                let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                if resp.hovered() && scroll != 0.0 {
                    *self.preview_zoom = (*self.preview_zoom * (1.0 - scroll * 0.002)).clamp(0.4, 4.0);
                }
                ui.horizontal(|ui| {
                    ui.toggle_value(self.preview_spinning, "⟲ spin");
                    ui.add(egui::Slider::new(self.preview_zoom, 0.4..=4.0).text("zoom"));
                });
            }
            Some(PreviewView::Image(handle, dims)) => {
                let max = 256.0;
                let (w, h) = (dims[0].max(1) as f32, dims[1].max(1) as f32);
                let s = (max / w.max(h)).min(1.0);
                ui.add(
                    egui::Image::new(&handle)
                        .fit_to_exact_size(egui::vec2(w * s, h * s))
                        .corner_radius(4.0),
                );
                ui.small(format!("{}×{} px", dims[0], dims[1]));
            }
            None => {
                ui.weak("(building preview…)");
            }
        }
    }

    /// Editable properties for a selected material preset, with a Save back to its
    /// `.ron`. Edits mutate the live preview material, so the sphere updates as you go.
    /// Per-texture sampling controls (filter + wrap), shown when a texture asset is
    /// selected. Changes are recorded on `cmd` and applied (persist + re-register)
    /// after the frame.
    pub(crate) fn texture_settings_ui(&mut self, ui: &mut egui::Ui, path: &str) {
        ui.separator();
        ui.strong("Sampling");
        let mut s = self.texture_settings.get(path).copied().unwrap_or_default();
        let before = s;
        egui::Grid::new("tex-sampling").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
            ui.label("filter");
            egui::ComboBox::from_id_salt("tex-filter")
                .selected_text(match s.filter {
                    FilterMode::Pixelated => "Pixelated",
                    FilterMode::Smooth => "Smooth",
                    FilterMode::SmoothMipmaps => "Smooth + Mipmaps",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut s.filter, FilterMode::Pixelated, "Pixelated");
                    ui.selectable_value(&mut s.filter, FilterMode::Smooth, "Smooth");
                    ui.selectable_value(&mut s.filter, FilterMode::SmoothMipmaps, "Smooth + Mipmaps");
                });
            ui.end_row();
            ui.label("wrap");
            egui::ComboBox::from_id_salt("tex-wrap")
                .selected_text(match s.wrap {
                    WrapMode::Repeat => "Repeat",
                    WrapMode::Clamp => "Clamp",
                    WrapMode::Mirror => "Mirror",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut s.wrap, WrapMode::Repeat, "Repeat");
                    ui.selectable_value(&mut s.wrap, WrapMode::Clamp, "Clamp");
                    ui.selectable_value(&mut s.wrap, WrapMode::Mirror, "Mirror");
                });
            ui.end_row();
        });
        ui.small("Pixelated = crisp · Smooth = bilinear · +Mipmaps = no shimmer at distance.");

        // ---- Spritesheet slicing -------------------------------------------
        ui.separator();
        ui.strong("Spritesheet");
        ui.small("Split this texture into a grid of cells. A UI image can then show one \
                  cell — animate the cell index for sprite animation.");
        let (mut cols, mut rows) = (s.sheet_cols.max(1), s.sheet_rows.max(1));
        ui.horizontal(|ui| {
            ui.label("columns");
            ui.add(egui::DragValue::new(&mut cols).range(1..=64));
            ui.label("rows");
            ui.add(egui::DragValue::new(&mut rows).range(1..=64));
            if ui.button("reset").on_hover_text("back to a single image").clicked() {
                cols = 1;
                rows = 1;
            }
        });
        // Non-1 grids persist; 1×1 stores 0 (the "no sheet" sentinel).
        s.sheet_cols = if cols > 1 || rows > 1 { cols } else { 0 };
        s.sheet_rows = if cols > 1 || rows > 1 { rows } else { 0 };
        // Preview: the texture with cell grid lines, and the cell count.
        if cols * rows > 1
            && let Some(crate::PreviewView::Image(handle, dims)) = &self.preview
        {
            let max = 200.0;
            let (w, h) = (dims[0].max(1) as f32, dims[1].max(1) as f32);
            let sc = (max / w.max(h)).min(1.0);
            let size = egui::vec2(w * sc, h * sc);
            let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
            egui::Image::new(handle).paint_at(ui, rect);
            let p = ui.painter_at(rect);
            let stroke = egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(255, 220, 80, 180));
            for c in 1..cols {
                let x = rect.left() + rect.width() * c as f32 / cols as f32;
                p.line_segment([egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())], stroke);
            }
            for r in 1..rows {
                let y = rect.top() + rect.height() * r as f32 / rows as f32;
                p.line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)], stroke);
            }
            ui.small(format!("{}×{} = {} cells (indexed row-major, 0..{})", cols, rows, cols * rows, cols * rows - 1));
        }
        if s != before {
            self.cmd.set_texture_setting = Some((path.to_string(), s));
        }
    }

    pub(crate) fn material_asset_ui(&mut self, ui: &mut egui::Ui, path: &str) {
        let Some((mpath, mat)) = self.preview_material.as_mut() else { return };
        if mpath != path {
            return;
        }
        ui.separator();
        let r = material_props_ui(ui, mat, self.materials, self.asset_tree, self.mat_name_buf, self.flsl_cache, self.sdf_cache);
        if let Some(name) = r.save_as
            && !name.is_empty() {
                self.cmd.save_material = Some((name, MaterialDoc::from_material(mat)));
            }
        if ui.button("Save to this preset").clicked() {
            let stem = Path::new(path)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if !stem.is_empty() {
                self.cmd.save_material = Some((stem, MaterialDoc::from_material(mat)));
            }
        }
    }
}
