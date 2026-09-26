//! Thumbnails of the project's own images, for the Assets tab and anything
//! else that shows a texture small: a material's swatch, a picker's row.
//!
//! Decoded off the UI thread, a few at a time, and shrunk to a thumbnail
//! before egui ever sees them — a folder of 4K textures scrolled past must not
//! stall a frame or hold its full-size pixels. A file is looked at again when
//! it changes on disk, so a texture painted in another program updates here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::SystemTime;

/// The longest edge a thumbnail is kept at.
const EDGE: u32 = 160;
/// Decodes running at once. More only makes each one arrive later.
const MAX_IN_FLIGHT: usize = 4;
/// Thumbnails held at once; past this the least recently shown is dropped.
const MAX_HELD: usize = 600;
/// The largest file decoded for a thumbnail. A texture bigger than this is a
/// glyph in the browser, which is a fair trade for never stalling on it.
const MAX_BYTES: u64 = 96 * 1024 * 1024;
/// How often (seconds) a held thumbnail's file is checked for changes.
const RECHECK_SECS: f64 = 2.0;

enum State {
    Loading(Receiver<Option<egui::ColorImage>>),
    Ready(egui::TextureHandle),
    /// Not an image we can read — kept, so it is not tried every frame.
    None,
}

struct Entry {
    state: State,
    stamp: Option<SystemTime>,
    checked_at: f64,
    used_at: f64,
}

#[derive(Default)]
pub(crate) struct AssetThumbs {
    held: HashMap<PathBuf, Entry>,
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn decode(path: &Path) -> Option<egui::ColorImage> {
    if std::fs::metadata(path).ok()?.len() > MAX_BYTES {
        return None;
    }
    let img = image::open(path).ok()?;
    let img = if img.width().max(img.height()) > EDGE { img.thumbnail(EDGE, EDGE) } else { img };
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Some(egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

impl AssetThumbs {
    /// The thumbnail of the image at `path`, starting its decode if this is
    /// the first ask. `None` while it is coming, or if it never will.
    pub(crate) fn get(&mut self, ctx: &egui::Context, path: &Path) -> Option<egui::TextureHandle> {
        let now = ctx.input(|i| i.time);
        let loading = self.held.values().filter(|e| matches!(e.state, State::Loading(_))).count();

        // Collect a finished decode.
        if let Some(entry) = self.held.get_mut(path)
            && let State::Loading(rx) = &entry.state
            && let Ok(img) = rx.try_recv()
        {
            entry.state = match img {
                Some(img) => State::Ready(ctx.load_texture(
                    format!("asset-thumb:{}", path.display()),
                    img,
                    egui::TextureOptions::LINEAR,
                )),
                None => State::None,
            };
        }

        // A changed file is decoded again.
        if let Some(entry) = self.held.get_mut(path)
            && !matches!(entry.state, State::Loading(_))
            && now - entry.checked_at > RECHECK_SECS
        {
            entry.checked_at = now;
            if modified(path) != entry.stamp {
                self.held.remove(path);
            }
        }

        if !self.held.contains_key(path) {
            if loading >= MAX_IN_FLIGHT {
                ctx.request_repaint();
                return None;
            }
            if self.held.len() >= MAX_HELD {
                self.evict_one();
            }
            let (tx, rx) = std::sync::mpsc::channel();
            let p = path.to_path_buf();
            std::thread::spawn(move || {
                let _ = tx.send(decode(&p));
            });
            self.held.insert(
                path.to_path_buf(),
                Entry { state: State::Loading(rx), stamp: modified(path), checked_at: now, used_at: now },
            );
        }

        let entry = self.held.get_mut(path)?;
        entry.used_at = now;
        match &entry.state {
            State::Ready(t) => Some(t.clone()),
            State::Loading(_) => {
                ctx.request_repaint();
                None
            }
            State::None => None,
        }
    }

    fn evict_one(&mut self) {
        let oldest = self
            .held
            .iter()
            .filter(|(_, e)| !matches!(e.state, State::Loading(_)))
            .min_by(|a, b| a.1.used_at.total_cmp(&b.1.used_at))
            .map(|(p, _)| p.clone());
        if let Some(p) = oldest {
            self.held.remove(&p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A texture shows as its own picture, shrunk; a file that is not an image
    /// settles as "no thumbnail" instead of being retried; and a texture
    /// repainted on disk is picked up again.
    #[test]
    fn thumbnails_arrive_shrunk_and_follow_the_file() {
        let dir = std::env::temp_dir().join(format!("floptle-thumbs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("big.png");
        image::RgbaImage::from_pixel(640, 320, image::Rgba([200, 30, 30, 255])).save(&png).unwrap();
        let junk = dir.join("junk.png");
        std::fs::write(&junk, b"not a png").unwrap();

        let ctx = egui::Context::default();
        let mut thumbs = AssetThumbs::default();
        // A wall-clock budget, not a count of short sleeps: the decode is a
        // worker's, and a loaded CI runner took longer than 500 × 5 ms.
        let settle = |thumbs: &mut AssetThumbs, p: &Path| {
            let started = std::time::Instant::now();
            while started.elapsed() < std::time::Duration::from_secs(30) {
                let mut out = None;
                let _ = ctx.run_ui(egui::RawInput::default(), |_| out = thumbs.get(&ctx, p));
                if out.is_some() || matches!(thumbs.held.get(p).map(|e| &e.state), Some(State::None)) {
                    return out;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("{} never settled", p.display());
        };
        let t = settle(&mut thumbs, &png).expect("a thumbnail");
        assert_eq!(t.size(), [EDGE as usize, EDGE as usize / 2], "shrunk, aspect kept");
        assert!(settle(&mut thumbs, &junk).is_none());

        // Repainted: a different size, noticed once the recheck comes round.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        image::RgbaImage::from_pixel(64, 64, image::Rgba([0, 0, 255, 255])).save(&png).unwrap();
        if let Some(e) = thumbs.held.get_mut(&png) {
            e.checked_at = f64::NEG_INFINITY;
        }
        let t = settle(&mut thumbs, &png).expect("the new picture");
        assert_eq!(t.size(), [64, 64]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
