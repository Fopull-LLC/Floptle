//! Per-OS locations and the Hub's persisted config (`hub.json`).

use crate::registry::Project;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The manifest that lists installable engine versions. Points at a GitHub-Releases asset
/// for now (private → the Hub adds an auth token); swappable to a public host later without
/// code changes (docs/hub-proposal.md §3.4).
pub const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/Fopull-LLC/Floptle/releases/download/manifest/releases.json";

/// Resolved per-OS directories. `data` holds `versions/` (installed bundles) + `cache/`
/// (downloaded archives); `config` holds `hub.json`.
#[derive(Clone, Debug)]
pub struct Paths {
    pub data: PathBuf,
    pub config: PathBuf,
}

impl Paths {
    /// The OS-conventional locations (`directories` crate). `None` if no home dir exists.
    pub fn resolve() -> Option<Self> {
        let pd = directories::ProjectDirs::from("com", "Fopull", "Floptle")?;
        Some(Self { data: pd.data_dir().to_path_buf(), config: pd.config_dir().to_path_buf() })
    }

    /// Explicit paths (for tests / a `--data-dir` override).
    pub fn at(root: &Path) -> Self {
        Self { data: root.join("data"), config: root.join("config") }
    }

    pub fn versions_dir(&self) -> PathBuf {
        self.data.join("versions")
    }
    pub fn cache_dir(&self) -> PathBuf {
        self.data.join("cache")
    }
    pub fn config_file(&self) -> PathBuf {
        self.config.join("hub.json")
    }
    /// The install dir for a specific version.
    pub fn version_dir(&self, version: &str) -> PathBuf {
        self.versions_dir().join(version)
    }

    /// Create the data/config/versions/cache dirs (idempotent).
    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.versions_dir())?;
        std::fs::create_dir_all(self.cache_dir())?;
        std::fs::create_dir_all(&self.config)?;
        Ok(())
    }
}

/// User-tweakable settings persisted in `hub.json`.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Settings {
    /// Release channel to show in Installs ("stable" | "beta").
    #[serde(default = "default_channel")]
    pub channel: String,
    /// The version launched for a project that pins none.
    #[serde(default)]
    pub default_version: Option<String>,
    /// Where to fetch the release manifest.
    #[serde(default = "default_manifest_url")]
    pub manifest_url: String,
    /// The parent folder new projects are created under. Remembered from the last create so
    /// the user isn't retyping a path each time; seeded with [`default_projects_dir`].
    #[serde(default)]
    pub projects_dir: Option<String>,
}

fn default_channel() -> String {
    "stable".to_string()
}
fn default_manifest_url() -> String {
    DEFAULT_MANIFEST_URL.to_string()
}

/// A sensible default parent for new projects: `~/Floptle Projects` (under the user's home),
/// falling back to the current dir if no home is known. Not created here — the create step
/// makes the project dir itself.
pub fn default_projects_dir() -> String {
    directories::UserDirs::new()
        .map(|u| u.home_dir().join("Floptle Projects"))
        .unwrap_or_else(|| PathBuf::from("."))
        .to_string_lossy()
        .into_owned()
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            channel: default_channel(),
            default_version: None,
            manifest_url: default_manifest_url(),
            projects_dir: None,
        }
    }
}

/// The Hub's persisted state.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct HubConfig {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default)]
    pub projects: Vec<Project>,
}

impl HubConfig {
    /// Load `hub.json`, or a default if it's missing/corrupt (never fails — a fresh user
    /// just gets defaults).
    pub fn load(paths: &Paths) -> Self {
        std::fs::read_to_string(paths.config_file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Persist `hub.json` (pretty-printed), creating the config dir if needed.
    pub fn save(&self, paths: &Paths) -> std::io::Result<()> {
        std::fs::create_dir_all(&paths.config)?;
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(paths.config_file(), text)
    }

    /// Add or update a project by path (keyed on path; refreshes name/version).
    pub fn upsert_project(&mut self, project: Project) {
        if let Some(existing) = self.projects.iter_mut().find(|p| p.path == project.path) {
            *existing = project;
        } else {
            self.projects.push(project);
        }
    }

    pub fn remove_project(&mut self, path: &Path) {
        self.projects.retain(|p| p.path != path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_and_defaults_on_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::at(tmp.path());
        // Missing file → defaults.
        let loaded = HubConfig::load(&paths);
        assert_eq!(loaded.settings.channel, "stable");
        assert!(loaded.projects.is_empty());

        let mut cfg = HubConfig::default();
        cfg.upsert_project(Project {
            name: "My Game".into(),
            path: PathBuf::from("/tmp/mygame"),
            engine_version: Some("0.3.0".into()),
            last_opened: None,
        });
        cfg.settings.default_version = Some("0.3.0".into());
        cfg.save(&paths).unwrap();

        let back = HubConfig::load(&paths);
        assert_eq!(back.projects.len(), 1);
        assert_eq!(back.projects[0].name, "My Game");
        assert_eq!(back.settings.default_version.as_deref(), Some("0.3.0"));
    }

    #[test]
    fn upsert_is_keyed_on_path() {
        let mut cfg = HubConfig::default();
        let p = PathBuf::from("/games/a");
        cfg.upsert_project(Project { name: "A".into(), path: p.clone(), engine_version: None, last_opened: None });
        cfg.upsert_project(Project { name: "A renamed".into(), path: p.clone(), engine_version: Some("0.2.0".into()), last_opened: None });
        assert_eq!(cfg.projects.len(), 1);
        assert_eq!(cfg.projects[0].name, "A renamed");
        cfg.remove_project(&p);
        assert!(cfg.projects.is_empty());
    }
}
