//! **An `.aseprite` file in the project is a sprite sheet and its clips.**
//!
//! Each `hero.aseprite` gets a `hero.aseprite.png` beside it, every frame on a
//! grid and the texture sliced to match, and one `.spriteanim.ron` per tag:
//! `hero_walk`, `hero_jump`, or a single `hero` when it has no tags. Saving in
//! Aseprite rewrites them within a second, and the texture hot reload puts the
//! new frames on screen. There is no export step and nothing to press.
//!
//! The outputs belong to the `.aseprite` file and are rewritten from it, so
//! change frames and timing in Aseprite, not in the clip.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::Editor;

const POLL_EVERY: Duration = Duration::from_millis(1000);

pub(crate) fn is_aseprite(path: &str) -> bool {
    let l = path.to_ascii_lowercase();
    l.ends_with(".aseprite") || l.ends_with(".ase")
}

/// The sheet an `.aseprite` file draws into.
pub(crate) fn sheet_path(src: &Path) -> PathBuf {
    let mut name = src.file_name().unwrap_or_default().to_os_string();
    name.push(".png");
    src.with_file_name(name)
}

/// Lay frames out left to right, eight to a row.
pub(crate) fn grid_of(n: usize) -> (u32, u32) {
    let cols = (n as u32).clamp(1, 8);
    (cols, (n as u32).div_ceil(cols).max(1))
}

fn collect(entries: &[crate::assets::AssetEntry], out: &mut Vec<PathBuf>) {
    for e in entries {
        match e {
            crate::assets::AssetEntry::Dir(_, kids) => collect(kids, out),
            crate::assets::AssetEntry::File { path, .. } if is_aseprite(path) => out.push(PathBuf::from(path)),
            _ => {}
        }
    }
}

impl Editor {
    /// Import every `.aseprite` whose sheet is missing or older than it.
    pub(crate) fn poll_aseprite_files(&mut self) {
        let now = Instant::now();
        if self.ase_poll_at.is_some_and(|t| now.duration_since(t) < POLL_EVERY) {
            return;
        }
        self.ase_poll_at = Some(now);
        let mut files = Vec::new();
        collect(&self.asset_tree, &mut files);
        for src in files {
            let Some(changed) = floptle_vfs::modified(&src) else { continue };
            let stale = floptle_vfs::modified(sheet_path(&src)).is_none_or(|out| out < changed);
            if stale {
                self.import_aseprite_file(&src);
            }
        }
    }

    /// Flatten `src` into its sheet and write its clips. Says what happened in
    /// the Console; returns whether it worked.
    pub(crate) fn import_aseprite_file(&mut self, src: &Path) -> bool {
        let stem = src.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let fail = |ed: &mut Editor, why: String| {
            ed.console.push(floptle_script::LogLevel::Error, format!("reading {stem}.aseprite: {why}"), None);
            false
        };
        let bytes = match floptle_vfs::read(src) {
            Ok(b) => b,
            Err(e) => return fail(self, e.to_string()),
        };
        let sprite = match crate::ase_file::read(&bytes) {
            Ok(s) => s,
            Err(why) => return fail(self, why),
        };
        let (cols, rows) = grid_of(sprite.frames.len());
        let (w, h) = (sprite.w, sprite.h);
        let mut pixels = vec![0u8; (cols * w * rows * h * 4) as usize];
        let stride = (cols * w * 4) as usize;
        for (i, f) in sprite.frames.iter().enumerate() {
            let (cx, cy) = (i as u32 % cols, i as u32 / cols);
            for y in 0..h {
                let from = (y * w * 4) as usize;
                let to = (cy * h + y) as usize * stride + (cx * w * 4) as usize;
                pixels[to..to + (w * 4) as usize].copy_from_slice(&f.rgba[from..from + (w * 4) as usize]);
            }
        }
        let tex = floptle_render::TextureData { pixels, width: cols * w, height: rows * h };
        let Some(png) = floptle_assets::encode_png(&tex) else {
            return fail(self, "the sheet would not encode as a PNG".into());
        };
        let sheet = sheet_path(src);
        if let Err(e) = floptle_vfs::write(&sheet, png) {
            return fail(self, format!("could not write {}: {e}", sheet.display()));
        }
        let tex_rel = crate::assets::asset_rel_path(&sheet.to_string_lossy(), &self.project_root);
        let mut setting = crate::assets::tex_setting(&self.texture_settings, &self.project_root, &tex_rel);
        if setting.sheet_cols != cols || setting.sheet_rows != rows {
            setting.sheet_cols = cols;
            setting.sheet_rows = rows;
            self.apply_texture_setting(&tex_rel, setting);
        }
        self.invalidate_texture(&sheet);

        let spans: Vec<(String, crate::ase_file::Tag)> = if sprite.tags.is_empty() {
            let all = crate::ase_file::Tag {
                name: String::new(),
                from: 0,
                to: sprite.frames.len() - 1,
                direction: crate::ase_file::Direction::Forward,
                repeat: 0,
            };
            vec![(stem.clone(), all)]
        } else {
            sprite
                .tags
                .iter()
                .map(|t| {
                    let safe: String = t
                        .name
                        .chars()
                        .map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
                        .collect();
                    (format!("{stem}_{safe}"), t.clone())
                })
                .collect()
        };
        let dir = src.parent().map(Path::to_path_buf).unwrap_or_else(|| self.project_root.clone());
        let mut written = Vec::new();
        for (name, t) in spans {
            let (order, looping) = crate::aseprite::play_order(t.from, t.to, t.direction, t.repeat);
            let seq: Vec<(u32, u32)> = order.iter().map(|&i| (i as u32, sprite.frames[i].ms)).collect();
            let mut doc = crate::aseprite::clip(&seq, looping, cols, rows);
            doc.texture = tex_rel.clone();
            let out = dir.join(format!("{name}{}", floptle_scene::SPRITE_ANIM_EXT));
            match floptle_scene::save_sprite_anim(&doc, &out) {
                Ok(()) => written.push(name),
                Err(e) => {
                    self.console.push(
                        floptle_script::LogLevel::Error,
                        format!("reading {stem}.aseprite: could not write {name}: {e}"),
                        None,
                    );
                }
            }
        }
        for w in &sprite.warnings {
            self.console.push(floptle_script::LogLevel::Warn, format!("{stem}.aseprite: {w}"), None);
        }
        self.anim.rescan(&self.project_root);
        self.asset_tree = crate::assets::build_assets(&self.project_root);
        self.console.push(
            floptle_script::LogLevel::Debug,
            format!(
                "{stem}.aseprite: {} frame(s) on {tex_rel} ({cols}×{rows}), clips {}",
                sprite.frames.len(),
                written.join(", ")
            ),
            None,
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use crate::ase_file::tests::{solid, Build};

    /// Drop an `.aseprite` in the project and the next poll writes its sheet,
    /// slices it, and writes a clip per tag; save it again and it is redone.
    #[test]
    fn an_aseprite_file_becomes_a_sheet_and_clips() {
        let dir = std::env::temp_dir().join(format!("floptle_ase_sync_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("art")).unwrap();
        let file = Build {
            w: 2,
            h: 2,
            layers: vec![("a", true, 0, false)],
            frames: (0..3).map(|i| (100u16, vec![(0u16, 0i16, 0i16, 2u16, 2u16, solid(2, 2, [i * 80, 0, 0, 255]))])).collect(),
            tags: vec![("idle", 0, 0, 0, 0), ("run", 1, 2, 0, 0)],
        };
        let src = dir.join("art/hero.aseprite");
        std::fs::write(&src, file.bytes()).unwrap();

        let mut ed = crate::Editor {
            project_root: dir.clone(),
            asset_tree: crate::assets::build_assets(&dir),
            ..Default::default()
        };
        ed.poll_aseprite_files();

        let sheet = image::open(dir.join("art/hero.aseprite.png")).expect("the sheet is written").to_rgba8();
        assert_eq!(sheet.dimensions(), (6, 2), "three 2×2 frames in a row");
        assert_eq!(sheet.get_pixel(4, 0).0, [160, 0, 0, 255], "frame 2 in the third cell");
        let s = crate::assets::tex_setting(&ed.texture_settings, &dir, "art/hero.aseprite.png");
        assert_eq!((s.sheet_cols, s.sheet_rows), (3, 1), "the texture is sliced to match");
        let run = floptle_scene::load_sprite_anim(&dir.join("art/hero_run.spriteanim.ron")).expect("a clip per tag");
        assert_eq!(run.frames.iter().map(|f| f.cell).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(run.texture, "art/hero.aseprite.png");
        assert!(dir.join("art/hero_idle.spriteanim.ron").exists());

        // An unchanged file is left alone; a newer one is read again.
        let before = std::fs::metadata(dir.join("art/hero.aseprite.png")).unwrap().modified().unwrap();
        ed.ase_poll_at = None;
        ed.poll_aseprite_files();
        let after = std::fs::metadata(dir.join("art/hero.aseprite.png")).unwrap().modified().unwrap();
        assert_eq!(before, after, "nothing changed, so nothing is rewritten");
        let mut more = file;
        more.frames.push((100, vec![(0, 0, 0, 2, 2, solid(2, 2, [0, 0, 255, 255]))]));
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&src, more.bytes()).unwrap();
        ed.ase_poll_at = None;
        ed.poll_aseprite_files();
        let sheet = image::open(dir.join("art/hero.aseprite.png")).unwrap().to_rgba8();
        assert_eq!(sheet.dimensions(), (8, 2), "the new frame is on the sheet");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
