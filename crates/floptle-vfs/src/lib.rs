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

/// Join a **relative** path onto `root` such that the result cannot leave it:
/// an absolute path, a `..`, a root or a drive prefix anywhere in `rel` is
/// refused with `None`. Purely lexical — nothing is read — so it answers the
/// same in a browser, for a bundle, and for a directory that does not exist
/// yet.
///
/// This is the one rule for every path a script or a scene file supplies and
/// the engine then opens on its behalf: the reference is relative to the
/// project, or it is not a reference.
pub fn contain(root: &Path, rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    if p.is_absolute() || rel.starts_with(['/', '\\']) {
        return None;
    }
    for c in p.components() {
        if matches!(
            c,
            std::path::Component::ParentDir
                | std::path::Component::RootDir
                | std::path::Component::Prefix(_)
        ) {
            return None;
        }
    }
    // A Windows spelling on any platform: `C:\x` is a prefix on Windows and
    // an ordinary file name everywhere else, where `Component::Prefix` never
    // fires — so it is refused by shape as well.
    let b = rel.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return None;
    }
    Some(root.join(p))
}

/// Is `candidate` inside `root`? Lexical, after both are made absolute against
/// the current directory and `.`/`..` are folded — so a relative project root
/// (`assets`) and an absolute reference into it compare as the same tree.
/// A path that climbs out through `..` and back in is judged by where it
/// lands. Symlinks are not followed: this asks where a path POINTS, not what
/// is at the other end, and it must answer for a file that is not there.
pub fn is_within(root: &Path, candidate: &Path) -> bool {
    normalize(candidate).starts_with(normalize(root))
}

/// Absolute and folded, lexically. A path that cannot be made absolute (no
/// current directory, as in a browser) is folded as it is — every path a
/// bundle serves is already absolute.
pub fn normalize(p: &Path) -> PathBuf {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd().map(|c| c.join(p)).unwrap_or_else(|| p.to_path_buf())
    };
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
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

    /// The rule for every script-supplied path: relative and inside, or refused.
    #[test]
    fn contain_refuses_every_way_out_of_the_root() {
        let root = Path::new("/proj");
        for bad in ["../etc/passwd", "/etc/passwd", "a/../../b", "..", "\\\\server\\share", "C:\\x", "c:/x", "/"] {
            assert_eq!(contain(root, bad), None, "{bad:?} escaped");
        }
        assert_eq!(contain(root, "sub/file.lua"), Some(PathBuf::from("/proj/sub/file.lua")));
        assert_eq!(contain(root, "a/./b"), Some(PathBuf::from("/proj/a/./b")));
        assert_eq!(contain(root, ""), Some(PathBuf::from("/proj")));
    }

    /// `is_within` judges where a path LANDS, whatever route it took.
    #[test]
    fn is_within_folds_dots_and_compares_absolute_trees() {
        let root = Path::new("/proj");
        assert!(is_within(root, Path::new("/proj/models/x.glb")));
        assert!(is_within(root, Path::new("/proj/a/../models/x.glb")));
        assert!(!is_within(root, Path::new("/proj/../etc/passwd")));
        assert!(!is_within(root, Path::new("/etc/passwd")));
        assert!(!is_within(root, Path::new("/project2/x")), "a sibling sharing a prefix");
        // A relative root and a relative candidate resolve against the same cwd.
        assert!(is_within(Path::new("assets"), Path::new("assets/textures/a.png")));
        assert!(!is_within(Path::new("assets"), Path::new("solar/scenes/x.ron")));
        assert!(!is_within(Path::new("assets"), Path::new("assets/../solar/x.ron")));
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
