//! Shared editor widgets.

use std::path::Path;

use crate::assets::{asset_kind_icon, is_texture, truncate_label, AssetEntry};

/// A rich asset picker: a searchable, foldered view of the project's asset tree
/// showing only files that pass `accept` (empty folders are pruned). Folders
/// collapse/expand (so a noisy sub-folder — e.g. VFX textures — can be folded
/// away), and the popup toggles between a condensed **list** and an **icon
/// grid** with live texture thumbnails. Search flattens the whole tree.
///
/// Persists its search text, layout choice, and (via egui) folder-collapse
/// state per `id`. Returns `Some(pick)` when something is chosen this frame —
/// `Some(None)` is the `none_label` entry, `Some(Some(path))` a file.
pub(crate) fn asset_picker(
    ui: &mut egui::Ui,
    id: egui::Id,
    selected_text: &str,
    none_label: Option<&str>,
    tree: &[AssetEntry],
    accept: fn(&str) -> bool,
    width: f32,
) -> Option<Option<String>> {
    let mut picked: Option<Option<String>> = None;
    egui::ComboBox::from_id_salt(id)
        .selected_text(selected_text.to_string())
        .width(width)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show_ui(ui, |ui| {
            ui.set_min_width(width.max(240.0));
            let qid = id.with("q");
            let gid = id.with("grid");
            let mut q: String = ui.data(|d| d.get_temp(qid).unwrap_or_default());
            let mut grid: bool = ui.data(|d| d.get_temp(gid).unwrap_or(false));
            // Header: search box + list/grid layout toggle.
            ui.horizontal(|ui| {
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut q)
                        .hint_text("🔍 search…")
                        .desired_width(width.max(240.0) - 64.0),
                );
                if ui.memory(|m| m.focused().is_none()) {
                    resp.request_focus();
                }
                if ui.selectable_label(!grid, "☰").on_hover_text("list").clicked() {
                    grid = false;
                }
                if ui.selectable_label(grid, "⊞").on_hover_text("grid").clicked() {
                    grid = true;
                }
            });
            ui.data_mut(|d| {
                d.insert_temp(qid, q.clone());
                d.insert_temp(gid, grid);
            });
            if none_label.is_some() || !q.is_empty() {
                ui.separator();
            }
            if let Some(nl) = none_label
                && ui.selectable_label(selected_text == nl, nl).clicked()
            {
                picked = Some(None);
                ui.data_mut(|d| d.insert_temp(qid, String::new()));
                ui.close();
            }
            let ql = q.to_lowercase();
            egui::ScrollArea::vertical().max_height(340.0).show(ui, |ui| {
                if ql.is_empty() {
                    picker_tree(ui, tree, accept, selected_text, grid, qid, &mut picked);
                } else {
                    // Flat, whole-tree search — folders don't matter here.
                    let mut hits: Vec<(String, String)> = Vec::new();
                    collect_matches(tree, accept, &ql, &mut hits);
                    hits.sort();
                    if hits.is_empty() {
                        ui.weak("no matches");
                    } else if grid {
                        ui.horizontal_wrapped(|ui| {
                            for (path, name) in &hits {
                                picker_tile(ui, path, name, selected_text, qid, &mut picked);
                            }
                        });
                    } else {
                        for (path, name) in &hits {
                            picker_row(ui, path, name, selected_text, qid, &mut picked);
                        }
                    }
                }
            });
        });
    picked
}

/// True if `entries` (recursively) hold at least one file passing `accept` — so
/// empty branches are pruned from the picker.
fn dir_has_match(entries: &[AssetEntry], accept: fn(&str) -> bool) -> bool {
    entries.iter().any(|e| match e {
        AssetEntry::Dir(_, kids) => dir_has_match(kids, accept),
        AssetEntry::File { path, .. } => accept(path),
    })
}

/// Gather every accepted file whose path matches the (lowercased) query.
fn collect_matches(
    entries: &[AssetEntry],
    accept: fn(&str) -> bool,
    ql: &str,
    out: &mut Vec<(String, String)>,
) {
    for e in entries {
        match e {
            AssetEntry::Dir(_, kids) => collect_matches(kids, accept, ql, out),
            AssetEntry::File { name, path } if accept(path) && path.to_lowercase().contains(ql) => {
                out.push((path.clone(), name.clone()));
            }
            AssetEntry::File { .. } => {}
        }
    }
}

/// Render the foldered tree: collapsible dirs (pruned to those with matches),
/// files as list rows or grid tiles.
fn picker_tree(
    ui: &mut egui::Ui,
    entries: &[AssetEntry],
    accept: fn(&str) -> bool,
    cur: &str,
    grid: bool,
    qid: egui::Id,
    picked: &mut Option<Option<String>>,
) {
    // Files of this level first (grid: wrapped tiles; list: rows), then folders.
    let files: Vec<(&str, &str)> = entries
        .iter()
        .filter_map(|e| match e {
            AssetEntry::File { name, path } if accept(path) => Some((path.as_str(), name.as_str())),
            _ => None,
        })
        .collect();
    if grid {
        ui.horizontal_wrapped(|ui| {
            for (path, name) in &files {
                picker_tile(ui, path, name, cur, qid, picked);
            }
        });
    } else {
        for (path, name) in &files {
            picker_row(ui, path, name, cur, qid, picked);
        }
    }
    for e in entries {
        if let AssetEntry::Dir(name, kids) = e
            && dir_has_match(kids, accept)
        {
            egui::CollapsingHeader::new(format!("🗀 {name}"))
                .id_salt(("pick-dir", name.as_str()))
                .default_open(true)
                .show(ui, |ui| {
                    picker_tree(ui, kids, accept, cur, grid, qid, picked);
                });
        }
    }
}

/// One condensed list row: icon + name, hover shows the full path.
fn picker_row(
    ui: &mut egui::Ui,
    path: &str,
    name: &str,
    cur: &str,
    qid: egui::Id,
    picked: &mut Option<Option<String>>,
) {
    let (icon, color) = asset_kind_icon(path);
    let sel = cur == name || cur == path;
    let text = egui::RichText::new(format!("{icon} {name}")).color(if sel {
        ui.visuals().selection.stroke.color
    } else {
        color.gamma_multiply(1.0)
    });
    if ui.selectable_label(sel, text).on_hover_text(path).clicked() {
        *picked = Some(Some(path.to_string()));
        ui.data_mut(|d| d.insert_temp(qid, String::new()));
        ui.close();
    }
}

/// One grid tile: a live texture thumbnail (or the type icon) over the name.
fn picker_tile(
    ui: &mut egui::Ui,
    path: &str,
    name: &str,
    cur: &str,
    qid: egui::Id,
    picked: &mut Option<Option<String>>,
) {
    let sel = cur == name || cur == path;
    let size = egui::vec2(72.0, 72.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let p = ui.painter_at(rect);
    let bg = if sel {
        ui.visuals().selection.bg_fill.gamma_multiply(0.5)
    } else if resp.hovered() {
        ui.visuals().widgets.hovered.bg_fill
    } else {
        ui.visuals().faint_bg_color
    };
    p.rect_filled(rect.shrink(2.0), 4.0, bg);
    if sel {
        p.rect_stroke(
            rect.shrink(2.0),
            4.0,
            egui::Stroke::new(1.5, ui.visuals().selection.stroke.color),
            egui::StrokeKind::Inside,
        );
    }
    let img_rect = egui::Rect::from_min_size(rect.min + egui::vec2(14.0, 6.0), egui::vec2(44.0, 44.0));
    if is_texture(path) && let Some(tex) = tex_thumb(ui, path) {
        egui::Image::new(&tex).paint_at(ui, img_rect);
    } else {
        let (icon, color) = asset_kind_icon(path);
        p.text(img_rect.center(), egui::Align2::CENTER_CENTER, icon, egui::FontId::proportional(26.0), color);
    }
    p.text(
        egui::pos2(rect.center().x, rect.bottom() - 10.0),
        egui::Align2::CENTER_CENTER,
        truncate_label(name, 12),
        egui::FontId::proportional(10.0),
        ui.visuals().text_color(),
    );
    let resp = resp.on_hover_text(path);
    if resp.clicked() {
        *picked = Some(Some(path.to_string()));
        ui.data_mut(|d| d.insert_temp(qid, String::new()));
        ui.close();
    }
}

/// A cached, downscaled egui thumbnail for a texture path (loaded from disk once,
/// then kept in egui memory keyed by path so the grid doesn't re-read every frame).
fn tex_thumb(ui: &egui::Ui, path: &str) -> Option<egui::TextureHandle> {
    let tid = egui::Id::new(("asset-thumb", path));
    if let Some(h) = ui.data(|d| d.get_temp::<egui::TextureHandle>(tid)) {
        return Some(h);
    }
    // Budget a few disk loads per frame so opening a big folder doesn't hitch;
    // unloaded tiles fall back to the icon and fill in over the next frames.
    let budget_id = egui::Id::new("asset-thumb-budget");
    let spent: u32 = ui.data(|d| d.get_temp(budget_id).unwrap_or(0));
    if spent >= 8 {
        ui.ctx().request_repaint();
        return None;
    }
    let img = floptle_assets::load_texture(Path::new(path))?;
    let color = downscale_rgba(&img.pixels, img.width as usize, img.height as usize, 48);
    let h = ui.ctx().load_texture(format!("thumb:{path}"), color, egui::TextureOptions::LINEAR);
    ui.data_mut(|d| {
        d.insert_temp(tid, h.clone());
        d.insert_temp(budget_id, spent + 1);
    });
    Some(h)
}

/// Box-downscale interleaved RGBA to at most `max` px on the long side — keeps
/// thumbnails tiny in GPU memory regardless of the source texture's size.
fn downscale_rgba(px: &[u8], w: usize, h: usize, max: usize) -> egui::ColorImage {
    if w == 0 || h == 0 {
        return egui::ColorImage::from_rgba_unmultiplied([1, 1], &[0, 0, 0, 0]);
    }
    let scale = (max as f32 / w.max(h) as f32).min(1.0);
    let (nw, nh) = ((w as f32 * scale).max(1.0) as usize, (h as f32 * scale).max(1.0) as usize);
    if nw == w && nh == h {
        return egui::ColorImage::from_rgba_unmultiplied([w, h], px);
    }
    let mut out = vec![0u8; nw * nh * 4];
    for y in 0..nh {
        let sy = y * h / nh;
        for x in 0..nw {
            let sx = x * w / nw;
            let si = (sy * w + sx) * 4;
            let di = (y * nw + x) * 4;
            out[di..di + 4].copy_from_slice(&px[si..si + 4]);
        }
    }
    egui::ColorImage::from_rgba_unmultiplied([nw, nh], &out)
}

/// A ComboBox over a long asset list with a search box. The search field
/// AUTO-FOCUSES when the popup opens (type immediately, no click needed),
/// clicking inside the popup does NOT close it (CloseOnClickOutside), and
/// picking an entry closes explicitly. Returns `Some(pick)` when something was
/// chosen this frame — `Some(None)` is the `none_label` entry.
///
/// Every long-list dropdown (textures, models) goes through here so they all
/// behave identically.
pub(crate) fn searchable_picker(
    ui: &mut egui::Ui,
    id: egui::Id,
    selected_text: &str,
    none_label: Option<&str>,
    items: &[String],
    width: f32,
) -> Option<Option<String>> {
    let mut picked: Option<Option<String>> = None;
    egui::ComboBox::from_id_salt(id)
        .selected_text(selected_text.to_string())
        .width(width)
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .show_ui(ui, |ui| {
            let sid = id.with("search");
            let mut q: String = ui.data_mut(|d| d.get_temp(sid).unwrap_or_default());
            let resp = ui.add(
                egui::TextEdit::singleline(&mut q).hint_text("🔍 search…").desired_width(f32::INFINITY),
            );
            // Grab the keyboard the moment the popup opens.
            if ui.memory(|m| m.focused().is_none()) {
                resp.request_focus();
            }
            ui.data_mut(|d| d.insert_temp(sid, q.clone()));
            let done = |ui: &mut egui::Ui| {
                ui.data_mut(|d| d.insert_temp(sid, String::new()));
                ui.close();
            };
            if let Some(nl) = none_label
                && ui.selectable_label(selected_text == nl, nl).clicked()
            {
                picked = Some(None);
                done(ui);
            }
            let ql = q.to_lowercase();
            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                for p in items.iter().filter(|p| ql.is_empty() || p.to_lowercase().contains(&ql)) {
                    let name = p.rsplit('/').next().unwrap_or(p);
                    if ui
                        .selectable_label(selected_text == name, name)
                        .on_hover_text(p.as_str())
                        .clicked()
                    {
                        picked = Some(Some(p.clone()));
                        done(ui);
                    }
                }
            });
        });
    picked
}
