//! The engine's view of files.
//!
//! On the desktop this crate is `std::fs` with the same names and one fewer
//! `std::` — every function delegates, and nothing about a native build changes
//! by going through it. In a browser there is no disk: the export packed the
//! game's project folder into one bundle ([`bundle`]), the page fetched it,
//! and [`mount`] made it the filesystem. Reads come from the bundle; what the
//! game writes (its saves) lands in an overlay that persists in the page's own
//! storage, so a slot survives a reload.
//!
//! The reason this is a crate and not a trait threaded through every loader:
//! there are a few hundred `std::fs` call sites in the engine half, each one
//! correct, and the browser build needs *all* of them to go somewhere else.
//! Same function names mean the change at each site is mechanical and the
//! native behaviour is provably unchanged. The browser CI gate
//! (`tools/web/clippy.toml`) then refuses `std::fs` in the engine half, so a
//! new read cannot quietly reach for the disk again.
//!
//! Two deliberate gaps, both named so nobody re-derives them:
//! - **The API is synchronous.** The whole bundle is in memory before the game
//!   starts; streaming assets in later is a feature with its own plan, not
//!   something v1 promises.
//! - **A browser has no modification times.** [`modified`] answers `None`
//!   there, and hot reload — which watches for a newer file — simply never
//!   fires, which is what a shipped build wants anyway.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

pub mod bundle;
#[cfg(any(target_arch = "wasm32", test))]
mod mem;

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(target_arch = "wasm32")]
mod web;

#[cfg(not(target_arch = "wasm32"))]
pub use native::*;
#[cfg(target_arch = "wasm32")]
pub use web::*;

pub use bundle::{Bundle, pack};

/// One entry of a directory listing — what [`read_dir`] yields.
///
/// Narrower than `std::fs::DirEntry` on purpose: a path and whether it is a
/// directory are what every listing in the engine asks, and they are the two
/// things a bundle can answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    path: PathBuf,
    is_dir: bool,
}

impl DirEntry {
    pub(crate) fn new(path: PathBuf, is_dir: bool) -> Self {
        Self { path, is_dir }
    }

    /// The entry's full path: the directory listed, joined with its name.
    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    /// The entry's own name within the directory.
    pub fn file_name(&self) -> OsString {
        self.path.file_name().map(OsString::from).unwrap_or_default()
    }

    pub fn is_dir(&self) -> bool {
        self.is_dir
    }

    pub fn is_file(&self) -> bool {
        !self.is_dir
    }
}

/// Read a whole file as UTF-8, the way `std::fs::read_to_string` does.
pub fn read_to_string<P: AsRef<Path>>(path: P) -> io::Result<String> {
    let bytes = read(path.as_ref())?;
    String::from_utf8(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// A file's mtime, if the platform has one. `None` in a browser, and for a
/// path that does not exist.
pub fn modified<P: AsRef<Path>>(path: P) -> Option<floptle_core::time::SystemTime> {
    modified_impl(path.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dir_entry_reports_its_name_and_kind() {
        let e = DirEntry::new(PathBuf::from("scenes/first.ron"), false);
        assert_eq!(e.file_name(), OsString::from("first.ron"));
        assert!(e.is_file() && !e.is_dir());
        assert_eq!(e.path(), PathBuf::from("scenes/first.ron"));
    }

    #[test]
    fn the_desktop_reads_the_real_disk() {
        let dir = std::env::temp_dir().join(format!("floptle-vfs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        write(&file, "hello").unwrap();
        assert!(exists(&file) && is_file(&file) && !is_dir(&file));
        assert!(is_dir(&dir));
        assert_eq!(read_to_string(&file).unwrap(), "hello");
        assert!(modified(&file).is_some());
        let names: Vec<_> = read_dir(&dir).unwrap().iter().map(DirEntry::file_name).collect();
        assert_eq!(names, vec![OsString::from("a.txt")]);
        remove_file(&file).unwrap();
        assert!(!exists(&file));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
