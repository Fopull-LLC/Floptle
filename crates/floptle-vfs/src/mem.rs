//! An in-memory filesystem: a read-only bundle under a writable overlay.
//!
//! This is the browser's filesystem, kept target-independent so its rules have
//! ordinary tests. The `web` module wraps it with persistence for the overlay;
//! nothing here knows what a page is.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use crate::DirEntry;
use crate::bundle::{Bundle, normalize};

pub(crate) struct MemFs {
    bundle: Bundle,
    /// Files written since mount — saves. Shadows the bundle, path for path.
    overlay: BTreeMap<String, Vec<u8>>,
}

pub(crate) fn key(path: &Path) -> String {
    normalize(&path.to_string_lossy())
}

fn not_found(path: &Path) -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, format!("{} is not in the game's bundle", path.display()))
}

impl MemFs {
    pub(crate) fn new(bundle: Bundle) -> Self {
        Self { bundle, overlay: BTreeMap::new() }
    }

    pub(crate) fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        let k = key(path);
        if let Some(v) = self.overlay.get(&k) {
            return Ok(v.clone());
        }
        self.bundle.get(&k).map(<[u8]>::to_vec).ok_or_else(|| not_found(path))
    }

    /// Write into the overlay. Returns the key, for whoever persists it.
    pub(crate) fn write(&mut self, path: &Path, contents: &[u8]) -> String {
        let k = key(path);
        self.overlay.insert(k.clone(), contents.to_vec());
        k
    }

    /// Restore an overlay entry (from persistence) without reporting it back.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn restore(&mut self, k: String, contents: Vec<u8>) {
        self.overlay.insert(k, contents);
    }

    /// A file's size without copying its bytes.
    pub(crate) fn size(&self, path: &Path) -> Option<u64> {
        let k = key(path);
        if let Some(v) = self.overlay.get(&k) {
            return Some(v.len() as u64);
        }
        self.bundle.get(&k).map(|b| b.len() as u64)
    }

    pub(crate) fn is_file(&self, path: &Path) -> bool {
        let k = key(path);
        self.overlay.contains_key(&k) || self.bundle.contains(&k)
    }

    pub(crate) fn is_dir(&self, path: &Path) -> bool {
        let k = key(path);
        if self.bundle.is_dir(&k) {
            return true;
        }
        if k.is_empty() {
            return true;
        }
        let prefix = format!("{k}/");
        self.overlay.range(prefix.clone()..).next().is_some_and(|(p, _)| p.starts_with(&prefix))
    }

    pub(crate) fn exists(&self, path: &Path) -> bool {
        self.is_file(path) || self.is_dir(path)
    }

    /// The direct children of `path`: files and sub-directories, each once,
    /// sorted by name — the union of the bundle and the overlay.
    pub(crate) fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        if !self.is_dir(path) {
            return Err(not_found(path));
        }
        let k = key(path);
        let prefix = if k.is_empty() { String::new() } else { format!("{k}/") };
        let mut files = BTreeSet::new();
        let mut dirs = BTreeSet::new();
        for p in self.bundle.paths().chain(self.overlay.keys().map(String::as_str)) {
            let Some(rest) = p.strip_prefix(&prefix) else { continue };
            match rest.split_once('/') {
                Some((dir, _)) => {
                    dirs.insert(dir.to_string());
                }
                None => {
                    files.insert(rest.to_string());
                }
            }
        }
        // The listed directory's own path is what the caller joined, so the
        // entries keep its spelling (a leading `/` included) rather than the key.
        let base: PathBuf = path.to_path_buf();
        let mut out: Vec<DirEntry> = dirs.into_iter().map(|d| DirEntry::new(base.join(&d), true)).collect();
        out.extend(files.into_iter().map(|f| DirEntry::new(base.join(&f), false)));
        Ok(out)
    }

    /// Remove an overlay file. Returns the key if something was removed. A
    /// bundled file cannot be removed — the bundle is the shipped game.
    pub(crate) fn remove_file(&mut self, path: &Path) -> io::Result<String> {
        let k = key(path);
        if self.overlay.remove(&k).is_some() {
            return Ok(k);
        }
        if self.bundle.contains(&k) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is part of the game's bundle and cannot be removed", path.display()),
            ));
        }
        Err(not_found(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::pack;

    fn fs() -> MemFs {
        MemFs::new(
            Bundle::parse(pack([
                ("project.ron", &b"()"[..]),
                ("scenes/first.ron", b"scene"),
                ("scenes/sub/deep.ron", b"deep"),
                ("save/old.ron", b"bundled save"),
            ]))
            .unwrap(),
        )
    }

    #[test]
    fn reads_come_from_the_bundle_until_a_write_shadows_them() {
        let mut m = fs();
        assert_eq!(m.read(Path::new("/scenes/first.ron")).unwrap(), b"scene");
        assert_eq!(m.read(Path::new("scenes/../scenes/first.ron")).unwrap(), b"scene");
        assert!(m.read(Path::new("/scenes/none.ron")).unwrap_err().kind() == io::ErrorKind::NotFound);
        m.write(Path::new("/scenes/first.ron"), b"edited");
        assert_eq!(m.read(Path::new("scenes/first.ron")).unwrap(), b"edited");
    }

    #[test]
    fn a_size_comes_from_the_bundle_or_the_overlay_without_reading() {
        let mut m = fs();
        assert_eq!(m.size(Path::new("/scenes/first.ron")), Some(5));
        assert_eq!(m.size(Path::new("/scenes")), None, "a directory has no size");
        assert_eq!(m.size(Path::new("/nope")), None);
        m.write(Path::new("/scenes/first.ron"), b"longer than five");
        assert_eq!(m.size(Path::new("/scenes/first.ron")), Some(16), "the overlay wins");
    }

    #[test]
    fn a_written_file_exists_and_its_parent_becomes_a_directory() {
        let mut m = fs();
        assert!(!m.exists(Path::new("/save/slot1.ron")));
        assert!(m.is_dir(Path::new("/save")));
        assert!(!m.is_dir(Path::new("/saves")));
        let k = m.write(Path::new("/saves/slot1.ron"), b"x");
        assert_eq!(k, "saves/slot1.ron");
        assert!(m.is_file(Path::new("/saves/slot1.ron")));
        assert!(m.is_dir(Path::new("/saves")));
        assert!(m.exists(Path::new("/saves")));
        assert!(!m.is_file(Path::new("/saves")));
    }

    #[test]
    fn a_listing_is_the_union_of_bundle_and_overlay_one_level_deep() {
        let mut m = fs();
        m.write(Path::new("/scenes/new.ron"), b"n");
        let names: Vec<(String, bool)> = m
            .read_dir(Path::new("/scenes"))
            .unwrap()
            .iter()
            .map(|e| (e.file_name().to_string_lossy().into_owned(), e.is_dir()))
            .collect();
        assert_eq!(
            names,
            vec![("sub".to_string(), true), ("first.ron".to_string(), false), ("new.ron".to_string(), false)]
        );
        let paths: Vec<PathBuf> = m.read_dir(Path::new("/scenes")).unwrap().iter().map(|e| e.path()).collect();
        assert_eq!(paths[0], PathBuf::from("/scenes/sub"));
        let root: Vec<String> = m
            .read_dir(Path::new("/"))
            .unwrap()
            .iter()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(root, vec!["save", "scenes", "project.ron"]);
        assert!(m.read_dir(Path::new("/nowhere")).is_err());
    }

    #[test]
    fn removing_forgets_a_save_but_refuses_the_bundle() {
        let mut m = fs();
        m.write(Path::new("/save/slot1.ron"), b"x");
        assert_eq!(m.remove_file(Path::new("/save/slot1.ron")).unwrap(), "save/slot1.ron");
        assert!(!m.exists(Path::new("/save/slot1.ron")));
        assert_eq!(m.remove_file(Path::new("/save/old.ron")).unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(m.remove_file(Path::new("/save/none.ron")).unwrap_err().kind(), io::ErrorKind::NotFound);
    }
}
