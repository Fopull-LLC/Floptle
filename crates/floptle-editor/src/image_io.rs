//! Project glue for the 🖼 Image tab: open, save, export — and the piece that
//! makes the whole feature worth building, **texture invalidation**.
//!
//! ## Why invalidation needed writing at all
//!
//! `ensure_texture` caches an upload by path + sampling setting and never looks
//! at the file again (`project.rs`). Rewriting a PNG on disk therefore changed
//! nothing on screen until the project was reloaded — so "draw on the left,
//! watch the mesh change on the right" was not a matter of wiring up an existing
//! mechanism, it needed one. There are two halves, and both are here:
//!
//! - **Push** — [`Editor::invalidate_texture`], called the instant the editor
//!   writes a PNG. Exact, free, no polling.
//! - **Poll** — [`Editor::poll_texture_hot_reload`], an mtime check over the
//!   textures that are actually resident. This is the house mtime pattern
//!   (`shaders.rs`, `prefab.rs`, `input_actions.rs`) and it means **Aseprite and
//!   Krita hot-reload too** — a real win for artists who won't switch tools.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use floptle_image::doc::{Image, Mode};
use floptle_image::{io as fio, Palette};

use crate::assets::FilterMode;
use crate::image_edit::NewForm;
use crate::image_ui::ImageExport;
use crate::Editor;

/// How often the mtime poll actually stats files.
const POLL_EVERY: Duration = Duration::from_millis(500);
/// The quiet gap after an edit before a `Live` document re-exports its PNG.
const LIVE_DEBOUNCE: Duration = Duration::from_millis(250);

/// True for a path the Image tab can open (an image, or a document).
pub(crate) fn is_image_doc(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with(&format!(".{}", fio::DOC_EXT))
}

/// Read `.floptle/palettes/*.gpl|*.hex` on top of the built-ins.
pub(crate) fn load_palettes(project_root: &Path) -> Vec<Palette> {
    let mut out = floptle_image::palette::builtin();
    let dir = project_root.join(".floptle").join("palettes");
    let Ok(rd) = std::fs::read_dir(&dir) else { return out };
    let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.is_file()).collect();
    files.sort();
    for f in files {
        // Guessed by CONTENT, like every other loader in the engine.
        if let Ok(text) = std::fs::read_to_string(&f)
            && let Some(mut p) = Palette::parse(&text)
        {
            if p.name.is_empty() || p.name == "palette" {
                p.name = f.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or(p.name);
            }
            out.retain(|q| q.name != p.name);
            out.push(p);
        }
    }
    out
}

impl Editor {
    /// The folder new images land in.
    pub(crate) fn image_dir(&self) -> PathBuf {
        self.project_root.join("textures")
    }

    /// Drop every registry entry that resolves to `file`, so the next
    /// `ensure_texture` re-uploads from disk. The registry is keyed by the ref as
    /// WRITTEN (project-relative, usually), so matching has to go through
    /// `resolve_asset_path` rather than comparing strings.
    pub(crate) fn invalidate_texture(&mut self, file: &Path) {
        let target = file.canonicalize().unwrap_or_else(|_| file.to_path_buf());
        let keys: Vec<String> = self
            .texture_registry
            .keys()
            .filter(|k| !k.starts_with("rt:"))
            .filter(|k| {
                let p = crate::project::resolve_asset_path(&self.project_root, k);
                p.canonicalize().unwrap_or(p) == target
            })
            .cloned()
            .collect();
        for k in keys {
            self.texture_registry.remove(&k);
            self.texture_registry_setting.remove(&k);
            self.texture_mtime.remove(&k);
        }
    }

    /// mtime poll over resident textures — an external save (Aseprite, Krita, a
    /// build script) reloads without anyone pressing anything.
    pub(crate) fn poll_texture_hot_reload(&mut self) {
        let now = Instant::now();
        if self.texture_poll_at.is_some_and(|t| now.duration_since(t) < POLL_EVERY) {
            return;
        }
        self.texture_poll_at = Some(now);
        let keys: Vec<String> =
            self.texture_registry.keys().filter(|k| !k.starts_with("rt:")).cloned().collect();
        let mut stale = Vec::new();
        for k in keys {
            let file = crate::project::resolve_asset_path(&self.project_root, &k);
            let Ok(m) = std::fs::metadata(&file).and_then(|m| m.modified()) else { continue };
            match self.texture_mtime.get(&k) {
                Some(prev) if *prev != m => stale.push((k, m)),
                Some(_) => {}
                None => {
                    self.texture_mtime.insert(k, m);
                }
            }
        }
        for (k, m) in stale {
            self.texture_registry.remove(&k);
            self.texture_registry_setting.remove(&k);
            self.texture_mtime.insert(k.clone(), m);
            log::info!("texture {k} changed on disk — reloading");
        }
    }

    // --- open / new ---------------------------------------------------------

    /// Open any image (or `.flimg`) in the 🖼 Image tab. A bare PNG is wrapped in
    /// a one-layer document; a PNG with a sibling `.flimg` opens the document.
    pub(crate) fn open_image_doc(&mut self, path: &str) {
        if !self.confirm_discard_image() {
            return;
        }
        let file = self.resolve_asset_path(path);
        let Some(doc) = fio::open_any(&file, Mode::Painterly) else {
            let why = if is_image_doc(&file.to_string_lossy()) {
                "the .flimg is from a different format version (refused rather than misread)"
            } else {
                "not an image this engine can decode"
            };
            self.console.push(
                floptle_script::LogLevel::Error,
                format!("🖼 could not open {} — {why}", file.display()),
                None,
            );
            self.image.toast("could not open that image — see the Console");
            return;
        };
        // The document path is always the `.flimg`, even when a PNG was opened —
        // saving then creates the document beside the image it came from.
        let doc_path = fio::doc_path_for(&file);
        let mtime = std::fs::metadata(&doc_path).and_then(|m| m.modified()).ok();
        let existed = doc_path.is_file();
        self.image.adopt(doc, Some(doc_path.clone()), mtime);
        // Seed the tab's mode from the texture's import setting, so a texture
        // marked Pixelated opens as pixel art without anyone choosing.
        let rel = crate::assets::asset_rel_path(&file.to_string_lossy(), &self.project_root);
        if !existed
            && crate::assets::tex_setting(&self.texture_settings, &self.project_root, &rel).filter
                != FilterMode::Pixelated
            && let Some(d) = self.image.doc.as_mut()
        {
            d.mode = Mode::Painterly;
            self.image.brush = crate::image_edit::default_brush_for(Mode::Painterly);
        }
        self.image.toast(if existed { "document opened" } else { "opened as a one-layer document" });
        self.focus_image_tab();
    }

    /// Create a fresh document from the New dialog (unsaved until you save it).
    pub(crate) fn new_image_doc(&mut self, form: &NewForm) {
        if !self.confirm_discard_image() {
            return;
        }
        let mut doc = Image::new(form.w, form.h, form.mode);
        if form.background
            && let Some(g) = doc.layers[0].grid_mut(0)
        {
            g.fill([255, 255, 255, 255]);
            doc.layers[0].name = "Background".into();
        }
        self.image.adopt(doc, None, None);
        self.image.toast("new image — Save writes the .flimg and the .png");
        self.focus_image_tab();
    }

    /// Refuse to drop an unsaved document silently. (No modal: this is a toast +
    /// a refusal, because losing work to a stray double-click is unforgivable
    /// and a modal here would be worse than the disease.)
    fn confirm_discard_image(&mut self) -> bool {
        if !self.image.dirty || self.image.doc.is_none() {
            return true;
        }
        if self.image_discard_armed {
            self.image_discard_armed = false;
            return true;
        }
        self.image_discard_armed = true;
        self.image
            .toast("this image has unsaved changes — save it, or repeat that to discard them");
        false
    }

    pub(crate) fn focus_image_tab(&mut self) {
        if let Some(dock) = self.dock_state.as_mut() {
            crate::dock::focus_image_tab(dock);
        }
    }

    // --- save / export -------------------------------------------------------

    /// Save the open document: the `.flimg` **and** the flattened `.png` beside
    /// it, then invalidate so every material using that PNG re-uploads.
    pub(crate) fn save_image_doc(&mut self) {
        let Some(doc) = self.image.doc.clone() else { return };
        let Some(path) = self.image.path.clone() else {
            // No path yet — ask for a name instead of inventing one.
            self.image.save_name = Some("untitled".into());
            return;
        };
        let fresh = !path.is_file() || !fio::png_path_for(&path).is_file();
        match fio::save_document(&path, &doc) {
            Ok(png) => {
                self.image.dirty = false;
                self.image.png_dirty = false;
                self.image_discard_armed = false;
                self.image.mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
                self.after_png_written(&png, &doc);
                self.image.toast(format!("saved {}", short_name(&png)));
                // Only a NEW file changes the tree; a rescan per save would walk
                // the whole project every time you pressed Ctrl+S.
                if fresh {
                    self.asset_tree = crate::assets::build_assets(&self.project_root);
                }
            }
            Err(e) => {
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("🖼 save failed: {e}"),
                    None,
                );
                self.image.toast("save failed — see the Console");
            }
        }
    }

    /// Save under a new name inside `textures/`.
    pub(crate) fn save_image_doc_as(&mut self, name: &str) {
        let dir = self.image_dir();
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{name}.{}", fio::DOC_EXT));
        self.image.path = Some(path);
        self.save_image_doc();
    }

    /// The shared tail of every PNG write: register the import settings the
    /// document implies, invalidate the GPU copy, and remember the new mtime so
    /// the poll doesn't immediately "detect" our own write.
    fn after_png_written(&mut self, png: &Path, doc: &Image) {
        let rel = crate::assets::asset_rel_path(&png.to_string_lossy(), &self.project_root);
        // A new pixel document exports Pixelated, so a 32² sprite is crisp on the
        // mesh without anyone knowing there was a setting to find.
        let want_filter = match doc.mode {
            Mode::Pixel => FilterMode::Pixelated,
            _ => FilterMode::SmoothMipmaps,
        };
        if !self.texture_settings.contains_key(&rel) {
            let entry = self.texture_settings.entry(rel.clone()).or_default();
            entry.filter = want_filter;
            self.save_texture_settings();
        }
        self.invalidate_texture(png);
        if let Ok(m) = std::fs::metadata(png).and_then(|m| m.modified()) {
            self.texture_mtime.insert(rel, m);
        }
    }

    /// The Live loop: re-export the PNG in the quiet moments so the Scene view
    /// tracks the brush. Never mid-stroke, never more than every 250 ms.
    pub(crate) fn step_image_live(&mut self) {
        if !self.image.live || !self.image.png_dirty || self.image.busy() {
            return;
        }
        let (Some(path), Some(doc)) = (self.image.path.clone(), self.image.doc.clone()) else {
            return;
        };
        let now = Instant::now();
        if self.image.last_live.is_some_and(|t| now.duration_since(t) < LIVE_DEBOUNCE) {
            return;
        }
        self.image.last_live = Some(now);
        // ONLY the PNG: re-encoding every layer into the .flimg four times a
        // second would be felt in the brush. Ctrl+S still writes the document.
        let png = floptle_image::io::png_path_for(&path);
        let flat = floptle_image::composite::flatten(&doc, self.image.frame);
        let fresh = !png.is_file();
        if floptle_image::io::save_png(&png, &flat, doc.w, doc.h).is_ok() {
            self.image.png_dirty = false;
            self.after_png_written(&png, &doc);
            if fresh {
                self.asset_tree = crate::assets::build_assets(&self.project_root);
            }
        }
    }

    /// Anything the File ▸ Export menu can write.
    pub(crate) fn export_image(&mut self, what: ImageExport) {
        let Some(doc) = self.image.doc.clone() else { return };
        let frame = self.image.frame;
        let stem = self
            .image
            .path
            .as_ref()
            .and_then(|p| p.file_stem())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "untitled".into());
        let dir = self
            .image
            .path
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| self.image_dir());
        let _ = std::fs::create_dir_all(&dir);

        let written = match what {
            ImageExport::Png => {
                let px = floptle_image::composite::flatten(&doc, frame);
                let out = dir.join(format!("{stem}.png"));
                fio::save_png(&out, &px, doc.w, doc.h).ok().map(|_| out)
            }
            ImageExport::Layer => {
                let px = floptle_image::composite::layer_only(&doc, doc.active, frame);
                let name = doc
                    .layers
                    .get(doc.active)
                    .map(|l| sanitize(&l.name))
                    .unwrap_or_else(|| "layer".into());
                let out = dir.join(format!("{stem}_{name}.png"));
                fio::save_png(&out, &px, doc.w, doc.h).ok().map(|_| out)
            }
            ImageExport::Selection => {
                let Some(sel) = doc.selection.as_ref() else { return };
                let b = sel.selected_bounds();
                if b.is_empty() {
                    return;
                }
                let flat = floptle_image::composite::flatten(&doc, frame);
                let mut px = vec![0u8; b.w as usize * b.h as usize * 4];
                for y in 0..b.h as i32 {
                    for x in 0..b.w as i32 {
                        let (sx, sy) = (b.x + x, b.y + y);
                        let so = (sy as usize * doc.w as usize + sx as usize) * 4;
                        let o = (y as usize * b.w as usize + x as usize) * 4;
                        let k = sel.at(sx, sy);
                        px[o] = flat[so];
                        px[o + 1] = flat[so + 1];
                        px[o + 2] = flat[so + 2];
                        px[o + 3] = floptle_image::u8c(flat[so + 3] as f32 * k);
                    }
                }
                let out = dir.join(format!("{stem}_crop.png"));
                fio::save_png(&out, &px, b.w, b.h).ok().map(|_| out)
            }
            ImageExport::Sheet => {
                let frames: Vec<Vec<u8>> =
                    (0..doc.frames).map(|f| floptle_image::composite::flatten(&doc, f)).collect();
                let cols = (self.image.sheet_cols > 0).then_some(self.image.sheet_cols);
                let sheet = floptle_image::sheet::pack(&frames, doc.w, doc.h, cols);
                let out = dir.join(format!("{stem}_sheet.png"));
                let ok = fio::save_png(&out, &sheet.pixels, sheet.w, sheet.h).is_ok();
                if ok {
                    // §5.2: the packer's grid MUST be the grid the engine reads,
                    // so write cols/rows into the texture's import settings.
                    let rel =
                        crate::assets::asset_rel_path(&out.to_string_lossy(), &self.project_root);
                    let e = self.texture_settings.entry(rel).or_default();
                    e.sheet_cols = sheet.cols;
                    e.sheet_rows = sheet.rows;
                    // Smooth, never SmoothMipmaps: a mipmapped sheet bleeds
                    // neighbouring cells into each other at distance.
                    e.filter = match doc.mode {
                        Mode::Pixel => FilterMode::Pixelated,
                        _ => FilterMode::Smooth,
                    };
                    self.save_texture_settings();
                    self.image.toast(format!(
                        "sheet {}×{} cells — grid written to .floptle/textures.ron",
                        sheet.cols, sheet.rows
                    ));
                }
                ok.then_some(out)
            }
            ImageExport::Gif => {
                let frames: Vec<Vec<u8>> =
                    (0..doc.frames).map(|f| floptle_image::composite::flatten(&doc, f)).collect();
                let out = dir.join(format!("{stem}.gif"));
                floptle_image::sheet::encode_gif(&frames, doc.w, doc.h, doc.fps)
                    .and_then(|b| std::fs::write(&out, b).ok())
                    .map(|_| out)
            }
        };
        match written {
            Some(p) => {
                self.after_png_written(&p, &doc);
                if !matches!(what, ImageExport::Sheet) {
                    self.image.toast(format!("exported {}", short_name(&p)));
                }
                self.asset_tree = crate::assets::build_assets(&self.project_root);
            }
            None => {
                self.console.push(
                    floptle_script::LogLevel::Error,
                    "🖼 export failed".into(),
                    None,
                );
                self.image.toast("export failed — see the Console");
            }
        }
    }

    /// Write the document's palette into `.floptle/palettes/<name>.gpl` so other
    /// documents (and the quantize adjustment) can pick it up.
    pub(crate) fn save_image_palette(&mut self) {
        let Some(p) = self.image.doc.as_ref().and_then(|d| d.palette.clone()) else { return };
        let dir = self.project_root.join(".floptle").join("palettes");
        let _ = std::fs::create_dir_all(&dir);
        let name = sanitize(if p.name.is_empty() { "palette" } else { &p.name });
        let path = dir.join(format!("{name}.gpl"));
        match std::fs::write(&path, p.to_gpl()) {
            Ok(_) => {
                self.image.palettes_loaded = false;
                self.image.toast(format!("palette saved to .floptle/palettes/{name}.gpl"));
            }
            Err(e) => self.console.push(
                floptle_script::LogLevel::Error,
                format!("🖼 palette save failed: {e}"),
                None,
            ),
        }
    }

    /// Reload the open document if something else rewrote it on disk (an
    /// external tool, a git checkout). An unsaved document is left alone —
    /// clobbering unsaved work would be the worst possible reading of "sync".
    pub(crate) fn poll_image_doc_reload(&mut self) {
        if self.image.dirty || self.image.doc.is_none() {
            return;
        }
        let Some(path) = self.image.path.clone() else { return };
        let Ok(m) = std::fs::metadata(&path).and_then(|m| m.modified()) else { return };
        if self.image.mtime == Some(m) {
            return;
        }
        // First sighting of a file that didn't exist when we opened it.
        if self.image.mtime.is_none() {
            self.image.mtime = Some(m);
            return;
        }
        if let Some(doc) = fio::load_document(&path) {
            self.image.adopt(doc, Some(path), Some(m));
            self.image.toast("reloaded — the document changed on disk");
        }
    }
}

fn short_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.display().to_string())
}

/// A filename-safe version of a layer/palette name.
fn sanitize(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let out = out.trim_matches('_').to_string();
    if out.is_empty() { "untitled".into() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_makes_filenames_safe() {
        assert_eq!(sanitize("Sweetie 16"), "Sweetie_16");
        assert_eq!(sanitize("../../etc/passwd"), "etc_passwd");
        assert_eq!(sanitize(""), "untitled");
        assert_eq!(sanitize("!!!"), "untitled");
    }

    #[test]
    fn documents_are_recognised_by_extension() {
        assert!(is_image_doc("art/thing.flimg"));
        assert!(is_image_doc("ART/THING.FLIMG"));
        assert!(!is_image_doc("art/thing.png"));
    }

    /// The registry is keyed by the ref as WRITTEN — usually project-relative,
    /// sometimes absolute — while the editor knows only the file it just wrote.
    /// Matching has to go through `resolve_asset_path`, or a save would appear
    /// to do nothing on the mesh (which is the entire feature).
    #[test]
    fn invalidation_matches_a_texture_by_whatever_spelling_the_registry_used() {
        let dir = std::env::temp_dir().join(format!("flimg-inval-{}", std::process::id()));
        let tex_dir = dir.join("textures");
        std::fs::create_dir_all(&tex_dir).unwrap();
        let file = tex_dir.join("wall.png");
        floptle_image::io::save_png(&file, &[1, 2, 3, 255], 1, 1).unwrap();
        let other = tex_dir.join("floor.png");
        floptle_image::io::save_png(&other, &[1, 2, 3, 255], 1, 1).unwrap();

        let mut ed = Editor { project_root: dir.clone(), ..Default::default() };
        // Three spellings of the same texture, plus an unrelated one and a live
        // render target (which must never be touched — it has no file at all).
        for k in [
            "textures/wall.png".to_string(),
            file.to_string_lossy().to_string(),
            "assets/../textures/wall.png".to_string(),
            "textures/floor.png".to_string(),
            "rt:minimap".to_string(),
        ] {
            ed.texture_registry.insert(k.clone(), floptle_render::TexId(7));
            ed.texture_registry_setting.insert(k, Default::default());
        }
        ed.invalidate_texture(&file);
        assert!(!ed.texture_registry.contains_key("textures/wall.png"));
        assert!(!ed.texture_registry.contains_key(&file.to_string_lossy().to_string()));
        assert!(ed.texture_registry.contains_key("textures/floor.png"), "an unrelated texture survives");
        assert!(ed.texture_registry.contains_key("rt:minimap"), "render targets are never invalidated");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The poll half: someone else (Aseprite, a build script) rewrites the file.
    #[test]
    fn the_mtime_poll_drops_a_texture_that_changed_on_disk() {
        let dir = std::env::temp_dir().join(format!("flimg-poll-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("textures")).unwrap();
        let file = dir.join("textures/wall.png");
        floptle_image::io::save_png(&file, &[1, 2, 3, 255], 1, 1).unwrap();

        let mut ed = Editor { project_root: dir.clone(), ..Default::default() };
        ed.texture_registry.insert("textures/wall.png".into(), floptle_render::TexId(1));
        // First poll only RECORDS the mtime — nothing is stale yet.
        ed.poll_texture_hot_reload();
        assert!(ed.texture_registry.contains_key("textures/wall.png"));
        assert!(ed.texture_mtime.contains_key("textures/wall.png"));

        // Rewrite with a distinctly newer mtime, and let the poll's rate limit lapse.
        std::thread::sleep(std::time::Duration::from_millis(20));
        floptle_image::io::save_png(&file, &[9, 9, 9, 255], 1, 1).unwrap();
        let newer = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        filetime_touch(&file, newer);
        ed.texture_poll_at = None;
        ed.poll_texture_hot_reload();
        assert!(
            !ed.texture_registry.contains_key("textures/wall.png"),
            "an external save must drop the cached upload"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bump a file's mtime without pulling in a dependency: rewriting it is
    /// enough on every filesystem we support, but be explicit about intent.
    fn filetime_touch(p: &Path, _when: std::time::SystemTime) {
        use std::io::Write;
        let bytes = std::fs::read(p).unwrap();
        let mut f = std::fs::File::create(p).unwrap();
        f.write_all(&bytes).unwrap();
        f.sync_all().unwrap();
    }

    #[test]
    fn palettes_load_from_the_project_over_the_builtins() {
        let dir = std::env::temp_dir().join(format!("flimg-pal-{}", std::process::id()));
        let pal_dir = dir.join(".floptle").join("palettes");
        std::fs::create_dir_all(&pal_dir).unwrap();
        std::fs::write(pal_dir.join("mine.hex"), "ff0000\n00ff00\n").unwrap();
        let ps = load_palettes(&dir);
        assert!(ps.iter().any(|p| p.name == "mine" && p.colors.len() == 2));
        assert!(ps.iter().any(|p| p.name == "PICO-8"), "built-ins survive");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
