//! `packages.ron` — what a *project* has installed.
//!
//! One file at the project root, meant to be committed. It records the identity,
//! the version, and **where each package came from**, which is the field that
//! makes a project reproducible: a teammate who clones the repo can be told
//! exactly what to fetch.
//!
//! ```ron
//! (
//!     packages: [
//!         (
//!             id: "com.example.grasstools",
//!             version: "1.2.0",
//!             source: Folder("/home/me/pkgs/grass"),
//!             enabled: true,
//!         ),
//!     ],
//! )
//! ```
//!
//! The files themselves live in `<project>/packages/<id>/` — except for a
//! `Linked` package, which is read in place from wherever it is being written.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::version::Version;

/// The folder installed packages live in, under the project root.
pub const PACKAGES_DIR: &str = "packages";
/// The installed list, at the project root.
pub const REGISTRY_FILE: &str = "packages.ron";

/// Where a package came from — and, for `Linked`, where it still is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Source {
    /// Copied in from a folder on this machine.
    Folder(String),
    /// Fetched from a Git remote. `rev` is a branch, tag or commit.
    Git { url: String, rev: Option<String> },
    /// Downloaded from the Floptle package registry.
    Registry,
    /// Written right here: `packages/<id>/` **is** the source, tracked with the
    /// project. What ✚ New Package makes.
    Authored,
    /// Read in place from a folder elsewhere on this machine — nothing was
    /// copied. This is the mode you develop a package in: edit the files where
    /// they live, and every project linked to them sees the change.
    Linked(String),
}

impl Source {
    /// One line for the package list.
    pub fn describe(&self) -> String {
        match self {
            Source::Folder(p) => format!("from {p}"),
            Source::Git { url, rev: Some(r) } => format!("from {url} @ {r}"),
            Source::Git { url, rev: None } => format!("from {url}"),
            Source::Registry => "from the Floptle registry".into(),
            Source::Authored => "written in this project".into(),
            Source::Linked(p) => format!("linked to {p}"),
        }
    }

    /// A linked package is not copied, so it is not the project's to delete.
    pub fn is_linked(&self) -> bool {
        matches!(self, Source::Linked(_))
    }
}

/// One installed package.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    /// The version installed, copied from the package's own manifest at install
    /// time. Kept here so the list reads without opening every package, and
    /// re-checked on load — a mismatch means somebody edited one of the two.
    pub version: Version,
    pub source: Source,
    /// Off = present, listed, and not loaded. Turning a package off is how you
    /// find out whether it is the one misbehaving, and it must not require
    /// deleting anything.
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

impl Entry {
    /// Where this package's files are, given the project root.
    ///
    /// A **relative** `Linked` path is relative to the project, not to wherever
    /// the editor happens to have been started from. Reading it as a working-
    /// directory path meant a link that worked when the editor was launched from
    /// a terminal in the project and silently found nothing when it was launched
    /// any other way — the same package, the same file, two answers.
    pub fn root_in(&self, project_root: &Path) -> PathBuf {
        match &self.source {
            Source::Linked(p) => {
                let p = Path::new(p);
                if p.is_absolute() { p.to_path_buf() } else { project_root.join(p) }
            }
            _ => project_root.join(PACKAGES_DIR).join(&self.id),
        }
    }
}

/// The parsed `packages.ron`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub packages: Vec<Entry>,
}

impl Registry {
    /// Read `<project>/packages.ron`. A project with no such file has no
    /// packages, which is not an error — it is every project until the first
    /// install.
    pub fn load(project_root: &Path) -> Result<Registry, String> {
        let path = project_root.join(REGISTRY_FILE);
        if !path.exists() {
            return Ok(Registry::default());
        }
        let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut reg: Registry = crate::manifest::ron_options()
            .from_str(&text)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        // Two entries claiming one id would give `find` an answer that depends
        // on file order. Keep the first and say so.
        let mut seen = std::collections::HashSet::new();
        let mut dupes = Vec::new();
        reg.packages.retain(|e| {
            if seen.insert(e.id.clone()) {
                true
            } else {
                dupes.push(e.id.clone());
                false
            }
        });
        if !dupes.is_empty() {
            return Err(format!(
                "{}: `{}` is listed more than once",
                path.display(),
                dupes.join("`, `")
            ));
        }
        Ok(reg)
    }

    /// Write `<project>/packages.ron`. Removing the last package removes the
    /// file rather than leaving an empty list behind — a project with no
    /// packages should look like one.
    pub fn save(&self, project_root: &Path) -> std::io::Result<()> {
        let path = project_root.join(REGISTRY_FILE);
        if self.packages.is_empty() {
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            return Ok(());
        }
        let cfg = ron::ser::PrettyConfig::new().struct_names(false);
        let text = ron::ser::to_string_pretty(self, cfg)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        std::fs::write(path, format!("{text}\n"))
    }

    pub fn find(&self, id: &str) -> Option<&Entry> {
        self.packages.iter().find(|e| e.id == id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut Entry> {
        self.packages.iter_mut().find(|e| e.id == id)
    }

    /// Add or replace an entry, keeping the list sorted by id so the file's
    /// diff is about what changed and not about install order.
    pub fn upsert(&mut self, entry: Entry) {
        match self.packages.iter_mut().find(|e| e.id == entry.id) {
            Some(slot) => *slot = entry,
            None => self.packages.push(entry),
        }
        self.packages.sort_by(|a, b| a.id.cmp(&b.id));
    }

    pub fn remove(&mut self, id: &str) -> Option<Entry> {
        let i = self.packages.iter().position(|e| e.id == id)?;
        Some(self.packages.remove(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("flpkg-reg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_project_with_no_file_has_no_packages() {
        let root = temp("empty");
        assert!(Registry::load(&root).unwrap().packages.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn round_trips() {
        let root = temp("round");
        let mut reg = Registry::default();
        reg.upsert(Entry {
            id: "com.example.b".into(),
            version: Version::new(1, 0, 0),
            source: Source::Folder("/tmp/b".into()),
            enabled: true,
        });
        reg.upsert(Entry {
            id: "com.example.a".into(),
            version: Version::new(0, 2, 0),
            source: Source::Git { url: "https://x/y.git".into(), rev: Some("v1".into()) },
            enabled: false,
        });
        reg.save(&root).unwrap();
        let back = Registry::load(&root).unwrap();
        assert_eq!(reg, back);
        // Sorted by id, not by insertion.
        assert_eq!(back.packages[0].id, "com.example.a");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn removing_the_last_one_removes_the_file() {
        let root = temp("lastone");
        let mut reg = Registry::default();
        reg.upsert(Entry {
            id: "com.example.a".into(),
            version: Version::new(1, 0, 0),
            source: Source::Authored,
            enabled: true,
        });
        reg.save(&root).unwrap();
        assert!(root.join(REGISTRY_FILE).exists());
        reg.remove("com.example.a");
        reg.save(&root).unwrap();
        assert!(!root.join(REGISTRY_FILE).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_duplicate_id_is_an_error_not_a_coin_flip() {
        let root = temp("dupe");
        std::fs::write(
            root.join(REGISTRY_FILE),
            r#"(packages: [
                (id: "com.a.b", version: "1.0.0", source: Authored, enabled: true),
                (id: "com.a.b", version: "2.0.0", source: Authored, enabled: true),
            ])"#,
        )
        .unwrap();
        let err = Registry::load(&root).unwrap_err();
        assert!(err.contains("com.a.b"), "{err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_linked_package_lives_where_it_is_written() {
        let e = Entry {
            id: "com.a.b".into(),
            version: Version::new(1, 0, 0),
            source: Source::Linked("/home/me/dev/pkg".into()),
            enabled: true,
        };
        assert_eq!(e.root_in(Path::new("/proj")), PathBuf::from("/home/me/dev/pkg"));
        let copied =
            Entry { source: Source::Folder("/elsewhere".into()), ..e.clone() };
        assert_eq!(copied.root_in(Path::new("/proj")), PathBuf::from("/proj/packages/com.a.b"));
    }

    #[test]
    fn enabled_defaults_to_true_for_a_hand_written_entry() {
        let root = temp("enabled");
        std::fs::write(
            root.join(REGISTRY_FILE),
            r#"(packages: [ (id: "com.a.b", version: "1.0.0", source: Authored) ])"#,
        )
        .unwrap();
        assert!(Registry::load(&root).unwrap().packages[0].enabled);
        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod root_tests {
    use super::*;

    /// A linked package is the mode somebody develops a package in, so "where
    /// are its files" has to answer the same way however the editor was started.
    #[test]
    fn a_relative_link_is_relative_to_the_project() {
        let project = Path::new("/home/me/MyGame");
        let rel = Entry {
            id: "com.me.kit".into(),
            version: Version::new(1, 0, 0),
            source: Source::Linked("packages/kit".into()),
            enabled: true,
        };
        assert_eq!(rel.root_in(project), PathBuf::from("/home/me/MyGame/packages/kit"));

        // An absolute link is left exactly as written — it is the whole point of
        // linking to somewhere else on the disk.
        let abs = Entry {
            source: Source::Linked("/work/shared/kit".into()),
            ..rel.clone()
        };
        assert_eq!(abs.root_in(project), PathBuf::from("/work/shared/kit"));

        // Everything else lives under the project's own packages folder.
        let installed = Entry { source: Source::Registry, ..rel };
        assert_eq!(
            installed.root_in(project),
            PathBuf::from("/home/me/MyGame").join(PACKAGES_DIR).join("com.me.kit")
        );
    }
}
