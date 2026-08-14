//! Thumbnails for the 📦 Packages catalogue.
//!
//! A catalogue of art is unbrowsable as a list of paragraphs. This is the piece
//! that lets the browser be a grid of pictures: fetch each row's `thumbnail`,
//! decode it, hand egui a texture, and remember which ones came to nothing so
//! the browser does not ask again every frame.
//!
//! # Everything happens off the UI thread, once
//!
//! A grid of forty rows would otherwise be forty blocking HTTPS requests inside
//! a frame. Each thumbnail is fetched on its own worker and collected when it
//! arrives; until then the cell draws its placeholder, which is a real state and
//! not a gap. **Every URL is asked for exactly once per session** — including
//! the ones that fail, because a 404 asked sixty times a second is a 404 asked
//! sixty times a second.
//!
//! # A thumbnail is untrusted input
//!
//! It is a file named by a stranger's manifest and served from a host they
//! chose. So: a size ceiling before decoding, a pixel ceiling after, and a hard
//! cap on how many are held at once. A package cannot make the editor spend an
//! unbounded amount of memory by pointing `thumbnail:` at something enormous.
//!
//! Local files are read for **installed** packages, which is what makes a
//! thumbnail work offline, and while an author is still writing the package that
//! has one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

/// The largest thumbnail this will download, in bytes. Generous for a picture
/// of a package and far below anything that could hurt.
const MAX_BYTES: usize = 4 * 1024 * 1024;

/// What a picture is being asked for, which decides how big it is kept.
///
/// The same file is wanted at two very different sizes and the difference
/// matters both ways: a grid cell held at gallery size is memory spent on
/// something 128px wide, and a gallery image held at grid size is a screenshot
/// nobody can read. A sky rendered across five phases at 1600px is exactly the
/// case — at 256 it is five smudges.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Detail {
    /// A cell in the browse grid.
    Grid,
    /// A gallery still, or one opened full size.
    Gallery,
}

impl Detail {
    /// The longest edge it is kept at. A 4000px hero image held at full size for
    /// forty rows is half a gigabyte for nothing.
    fn max_edge(self) -> u32 {
        match self {
            Detail::Grid => 256,
            Detail::Gallery => 1600,
        }
    }

    /// How many are held at once. Past this, new ones stop being fetched rather
    /// than evicting something on screen — thrashing is worse than a few
    /// placeholders. Gallery images are ~25x the pixels, and only one package's
    /// gallery is ever on screen, so it is allowed far fewer.
    fn max_held(self) -> usize {
        match self {
            Detail::Grid => 400,
            Detail::Gallery => 32,
        }
    }

    fn slot(self) -> usize {
        match self {
            Detail::Grid => 0,
            Detail::Gallery => 1,
        }
    }
}

/// What one thumbnail is doing.
enum Thumb {
    Fetching(Receiver<Result<egui::ColorImage, String>>),
    Ready(egui::TextureHandle),
    /// It could not be had. Kept as a state so it is not asked for again — and
    /// with the reason, because "this package has no picture" and "this
    /// package's picture 404s" are different things to its author.
    Failed(String),
}

#[derive(Default)]
pub(crate) struct Thumbs {
    /// One map per [`Detail`], because the same URL is a different picture in
    /// each and they must not evict one another.
    by_url: [HashMap<String, Thumb>; 2],
}

impl Thumbs {
    /// The texture for `src`, starting a fetch if this is the first ask.
    ///
    /// `base` is the folder to resolve a package-relative path against — the
    /// installed package's root. `None` means only absolute URLs can be
    /// resolved, which is the catalogue's case.
    ///
    /// Returns `None` while it is still coming, or if it never will.
    pub(crate) fn get(
        &mut self,
        ctx: &egui::Context,
        src: &str,
        base: Option<&Path>,
        detail: Detail,
    ) -> Option<&egui::TextureHandle> {
        let key = resolve(src, base)?;
        let held = &mut self.by_url[detail.slot()];

        // Collect anything that has arrived since the last frame.
        if let Some(Thumb::Fetching(rx)) = held.get(&key)
            && let Ok(result) = rx.try_recv()
        {
            let next = match result {
                Ok(img) => Thumb::Ready(ctx.load_texture(
                    format!("pkgthumb:{}:{key}", detail.slot()),
                    img,
                    egui::TextureOptions::LINEAR,
                )),
                Err(e) => Thumb::Failed(e),
            };
            held.insert(key.clone(), next);
        }

        if !held.contains_key(&key) {
            if held.len() >= detail.max_held() {
                return None;
            }
            held.insert(key.clone(), Thumb::Fetching(load(key.clone(), detail.max_edge())));
        }
        match held.get(&key) {
            Some(Thumb::Ready(t)) => Some(t),
            _ => None,
        }
    }

    /// Why `src` has no picture, if it was tried and failed. Shown to an author
    /// looking at their own package, so a broken `thumbnail:` path is something
    /// they can see rather than guess at.
    pub(crate) fn failure(&self, src: &str, base: Option<&Path>) -> Option<&str> {
        let key = resolve(src, base)?;
        match self.by_url[Detail::Grid.slot()].get(&key) {
            Some(Thumb::Failed(e)) => Some(e),
            _ => None,
        }
    }
}

/// A manifest reference turned into something loadable: an absolute URL, or an
/// absolute path under `base`.
///
/// `None` when it cannot be resolved *or would escape the package* — a
/// `thumbnail:` of `../../../.ssh/id_rsa` must not be read, never mind drawn.
/// The manifest validator already refuses that, but this is the code that opens
/// the file and a package could be installed by a route that never validated.
fn resolve(src: &str, base: Option<&Path>) -> Option<String> {
    let src = src.trim();
    if src.is_empty() {
        return None;
    }
    if floptle_package::manifest::is_url(src) {
        return Some(src.to_string());
    }
    let base = base?;
    let joined = base.join(src);
    // Resolve `..` textually; a package folder may not exist yet on disk in the
    // case that matters (an author mid-write), so canonicalize is not usable.
    let mut depth: i32 = 0;
    for c in Path::new(src).components() {
        match c {
            std::path::Component::ParentDir => depth -= 1,
            std::path::Component::CurDir => {}
            std::path::Component::Normal(_) => depth += 1,
            // Absolute, or a Windows prefix — not package-relative at all.
            _ => return None,
        }
        if depth < 0 {
            return None;
        }
    }
    Some(joined.to_string_lossy().to_string())
}

/// Fetch and decode one thumbnail on its own thread.
fn load(key: String, max_edge: u32) -> Receiver<Result<egui::ColorImage, String>> {
    let (tx, rx): (Sender<Result<egui::ColorImage, String>>, _) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(read_bytes(&key).and_then(|b| decode(&b, max_edge)));
    });
    rx
}

fn read_bytes(key: &str) -> Result<Vec<u8>, String> {
    if floptle_package::manifest::is_url(key) {
        let resp = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .get(key)
            .call()
            .map_err(|e| format!("could not fetch: {e}"))?;
        let mut buf = Vec::new();
        // `take` before reading, so a server that streams forever cannot fill
        // memory — the ceiling is enforced on the way in, not checked after.
        std::io::Read::read_to_end(
            &mut std::io::Read::take(resp.into_reader(), MAX_BYTES as u64 + 1),
            &mut buf,
        )
        .map_err(|e| format!("could not read: {e}"))?;
        if buf.len() > MAX_BYTES {
            return Err(format!("larger than {} MB", MAX_BYTES / 1024 / 1024));
        }
        Ok(buf)
    } else {
        let meta = std::fs::metadata(PathBuf::from(key))
            .map_err(|e| format!("could not open: {e}"))?;
        if meta.len() as usize > MAX_BYTES {
            return Err(format!("larger than {} MB", MAX_BYTES / 1024 / 1024));
        }
        std::fs::read(key).map_err(|e| format!("could not read: {e}"))
    }
}

fn decode(bytes: &[u8], max_edge: u32) -> Result<egui::ColorImage, String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("not an image: {e}"))?;
    let img = if img.width().max(img.height()) > max_edge {
        img.thumbnail(max_edge, max_edge)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absolute_url_resolves_to_itself_with_or_without_a_base() {
        let url = "https://example.com/a.png";
        assert_eq!(resolve(url, None).as_deref(), Some(url));
        assert_eq!(resolve(url, Some(Path::new("/pkg"))).as_deref(), Some(url));
        assert_eq!(resolve("http://example.com/a.png", None).as_deref(),
                   Some("http://example.com/a.png"));
    }

    #[test]
    fn a_relative_path_needs_a_package_to_be_relative_to() {
        assert_eq!(resolve("media/icon.png", None), None);
        assert_eq!(
            resolve("media/icon.png", Some(Path::new("/pkg"))).as_deref(),
            Some("/pkg/media/icon.png")
        );
    }

    /// The manifest validator refuses these, but this is the code that opens the
    /// file — and a package can arrive by a route that never validated.
    #[test]
    fn a_thumbnail_may_not_point_outside_its_own_package() {
        let base = Some(Path::new("/pkg"));
        assert_eq!(resolve("../../.ssh/id_rsa", base), None);
        assert_eq!(resolve("media/../../secret", base), None);
        assert_eq!(resolve("/etc/passwd", base), None);
        // Climbing and coming back is still inside, and is fine.
        assert_eq!(
            resolve("media/../icon.png", base).as_deref(),
            Some("/pkg/media/../icon.png")
        );
    }

    #[test]
    fn nothing_is_not_a_thumbnail() {
        assert_eq!(resolve("", Some(Path::new("/pkg"))), None);
        assert_eq!(resolve("   ", Some(Path::new("/pkg"))), None);
    }

    /// A picture that is not a picture must be a failed thumbnail, not a panic
    /// on the worker thread.
    #[test]
    fn a_file_that_is_not_an_image_fails_in_words() {
        let err = decode(b"this is not a png", Detail::Grid.max_edge()).unwrap_err();
        assert!(err.contains("not an image"), "{err}");
    }

    /// The ceiling is what stops a stranger's manifest costing an unbounded
    /// amount of memory per row — and there are two of them, because a gallery
    /// still kept at grid size is a screenshot nobody can read.
    #[test]
    fn a_large_image_is_kept_small_at_the_size_it_was_asked_for() {
        let big = image::RgbaImage::from_pixel(4000, 2000, image::Rgba([9, 9, 9, 255]));
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(big)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        for detail in [Detail::Grid, Detail::Gallery] {
            let out = decode(png.get_ref(), detail.max_edge()).unwrap();
            assert!(out.size[0] <= detail.max_edge() as usize, "{:?}", out.size);
            assert!(out.size[1] <= detail.max_edge() as usize, "{:?}", out.size);
            // …and the aspect ratio is kept, so nothing is squashed into the cell.
            assert!(out.size[0] > out.size[1]);
        }
        // The gallery really is bigger — a shared ceiling would make this whole
        // distinction a no-op that nothing would notice.
        let grid = decode(png.get_ref(), Detail::Grid.max_edge()).unwrap();
        let gallery = decode(png.get_ref(), Detail::Gallery.max_edge()).unwrap();
        assert!(gallery.size[0] > grid.size[0] * 2, "{:?} vs {:?}", gallery.size, grid.size);
    }
}
