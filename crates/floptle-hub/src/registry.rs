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
    /// UTC ISO-8601 stamp (`2026-09-23T14:05:09Z`) of the last time the Hub opened,
    /// created or added it. Fixed width, so the strings sort in time order. The caller
    /// supplies "now" (see [`iso8601_utc`]) so the core stays deterministic.
    #[serde(default)]
    pub last_opened: Option<String>,
}

/// `unix_secs` as a UTC ISO-8601 stamp, `YYYY-MM-DDTHH:MM:SSZ`.
pub fn iso8601_utc(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    // Days since the epoch to a civil date (Howard Hinnant's `civil_from_days`).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem / 60 % 60,
        rem % 60
    )
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

/// Scan `versions/` for installed bundles. The version comes from the bundle's
/// own `version.json` when present — so a hand-unpacked archive counts as a
/// real install whatever its folder is called (an install named after a
/// `floptle-0.1.3-linux-x86_64/` folder matches no release and reads as
/// "not installed").
/// Version-named dirs without a `version.json` still count (legacy bundles).
/// Skips hidden/staging dirs (`.staging-*`); duplicates of one version keep
/// the first found. A broken install (missing binary) is still listed so the
/// UI can flag it and offer Uninstall. Sorted by
/// [`crate::releases::version_key`] so newest sorts last.
pub fn scan_installs(versions_dir: &Path) -> Vec<Install> {
    let mut out: Vec<Install> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(versions_dir) {
        for e in rd.flatten() {
            let path = e.path();
            if !path.is_dir() {
                continue;
            }
            let Some(name) = e.file_name().to_str().map(String::from) else { continue };
            if name.starts_with('.') {
                continue;
            }
            let from_json = std::fs::read_to_string(path.join("version.json"))
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .and_then(|v| v.get("version").and_then(|x| x.as_str()).map(String::from))
                .filter(|v| !v.is_empty());
            let looks_versioned = name.chars().next().is_some_and(|c| c.is_ascii_digit());
            let version = match (from_json, looks_versioned) {
                (Some(v), _) => v,
                (None, true) => name,
                (None, false) => continue, // stray non-bundle dir
            };
            if !out.iter().any(|i| i.version == version) {
                out.push(Install { version, path });
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
        // Semver-ish: 0.2 < 0.9 < 0.10 (not lexical, which would put 0.10 first).
        assert_eq!(versions, ["0.2.0", "0.9.0", "0.10.0"]);
    }

    /// A hand-unpacked archive (folder named after the archive, not the
    /// version) registers under its version.json's version — a developer unpacked
    /// `floptle-0.1.3-linux-x86_64/` manually and the Hub called it not
    /// installed. Stray dirs stay ignored; duplicates keep the first.
    #[test]
    fn scan_reads_version_json_over_the_folder_name() {
        let tmp = tempfile::tempdir().unwrap();
        let v = tmp.path().join("versions");
        let hand = v.join("floptle-0.1.3-linux-x86_64");
        std::fs::create_dir_all(&hand).unwrap();
        std::fs::write(
            hand.join("version.json"),
            br#"{ "version": "0.1.3", "target": "linux-x86_64", "commit": "abc1234" }"#,
        )
        .unwrap();
        std::fs::create_dir_all(v.join("random-notes")).unwrap(); // no digits, no json
        let installs = scan_installs(&v);
        assert_eq!(installs.len(), 1);
        assert_eq!(installs[0].version, "0.1.3");
        assert_eq!(installs[0].path, hand);
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
