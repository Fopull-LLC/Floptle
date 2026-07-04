//! The things the Hub tracks: **projects** (where the user makes games, referenced by
//! path) and **installs** (unpacked engine bundles under `versions/`).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A project the Hub tracks. It lives wherever the user created it; the Hub only holds a
/// reference and re-validates that the directory still exists. `engine_version` here is a
/// cache — the authority is the `engine_version` in the project's own `project.ron`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Project {
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub engine_version: Option<String>,
    /// ISO-8601 stamp of the last launch (the Hub never computes dates itself — the
    /// caller supplies "now" so the core stays testable/deterministic).
    #[serde(default)]
    pub last_opened: Option<String>,
}

impl Project {
    /// The directory still exists on disk.
    pub fn exists(&self) -> bool {
        self.path.is_dir()
    }

    /// The engine version pinned in the project's `project.ron` (the source of truth —
    /// survives a Hub registry reset or moving the project between machines). `None` if the
    /// project predates the Hub or has no config.
    pub fn pinned_version(&self) -> Option<String> {
        floptle_scene::load_project(&self.path.join("project.ron")).engine_version
    }

    /// Re-read the cached `engine_version` from `project.ron`.
    pub fn refresh(&mut self) {
        self.engine_version = self.pinned_version();
    }
}

/// An unpacked engine bundle under `versions/<version>/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Install {
    pub version: String,
    pub path: PathBuf,
}

impl Install {
    /// The editor executable inside this bundle (flat layout: the binary at the bundle
    /// root; `.app` bundling is a later packaging concern).
    pub fn editor_bin(&self) -> PathBuf {
        self.path.join(editor_bin_name())
    }

    /// The bundle is usable (its editor binary is present).
    pub fn is_valid(&self) -> bool {
        self.editor_bin().is_file()
    }
}

/// The editor executable's file name within a bundle, per OS. The `floptle-editor` crate's
/// `[[bin]]` is named `floptle`, so that's what a bundle ships and the Hub launches.
pub fn editor_bin_name() -> &'static str {
    if cfg!(windows) { "floptle.exe" } else { "floptle" }
}

/// Scan `versions/` for installed bundles — one per subdirectory named by its version.
/// Sorted by [`crate::releases::version_key`] so newest sorts last.
pub fn scan_installs(versions_dir: &Path) -> Vec<Install> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(versions_dir) {
        for e in rd.flatten() {
            if e.path().is_dir()
                && let Some(name) = e.file_name().to_str().map(String::from)
            {
                out.push(Install { version: name, path: e.path() });
            }
        }
    }
    out.sort_by_key(|i| crate::releases::version_key(&i.version));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_finds_version_dirs_sorted() {
        let tmp = tempfile::tempdir().unwrap();
        let v = tmp.path().join("versions");
        for name in ["0.10.0", "0.2.0", "0.9.0"] {
            std::fs::create_dir_all(v.join(name)).unwrap();
        }
        std::fs::write(v.join("not-a-dir"), b"x").unwrap();
        let installs = scan_installs(&v);
        let versions: Vec<&str> = installs.iter().map(|i| i.version.as_str()).collect();
        // Semver-ish: 0.2 < 0.9 < 0.10 (NOT lexical, which would put 0.10 first).
        assert_eq!(versions, ["0.2.0", "0.9.0", "0.10.0"]);
    }

    #[test]
    fn install_validity_checks_the_editor_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = Install { version: "0.1.0".into(), path: tmp.path().to_path_buf() };
        assert!(!inst.is_valid(), "no binary yet");
        std::fs::write(inst.editor_bin(), b"#!/bin/sh\n").unwrap();
        assert!(inst.is_valid());
    }
}
