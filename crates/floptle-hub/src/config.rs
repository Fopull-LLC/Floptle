//! Per-OS locations and the Hub's persisted config (`hub.json`).

use crate::registry::Project;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The manifest that lists installable engine versions — shared with the editor's
/// export templates, which resolve against the very same releases.
pub use floptle_dist::DEFAULT_MANIFEST_URL;

/// The pre-public default — configs that still carry it migrate to
/// [`DEFAULT_MANIFEST_URL`] on load (a hand-customized URL is left alone).
const LEGACY_MANIFEST_URL: &str =
    "https://github.com/Fopull-LLC/Floptle/releases/download/manifest/releases.json";

/// The Phase-1 dev provider. It was a hand-run script over a throwaway sqlite database; it is
/// switched off and it is not coming back, so a config still holding it can only ever fail to
/// sign in. Treated as a **known-dead value** rather than as a URL somebody might have meant:
/// anything else the user typed is still respected.
const RETIRED_AUTH_HOST: &str = "dev-auth.fopull.com";

/// Resolved per-OS directories. `data` holds `versions/` (installed bundles) + `cache/`
/// (downloaded archives); `config` holds `hub.json`.
#[derive(Clone, Debug)]
pub struct Paths {
    pub data: PathBuf,
    pub config: PathBuf,
}

impl Paths {
    /// The OS-conventional locations. `None` if no home dir exists. Resolved through
    /// `floptle-dist` so the editor's export templates land beside these installs.
    pub fn resolve() -> Option<Self> {
        Some(Self { data: floptle_dist::data_dir()?, config: floptle_dist::config_dir()? })
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
    /// The OAuth/OIDC provider the Hub signs into (fopull.com in production; point it at a
    /// dev instance to test the flow). The account token is only ever sent here.
    #[serde(default = "default_auth_base_url")]
    pub auth_base_url: String,
    /// The newest version the user dismissed the update banner for — the
    /// banner stays hidden for that version and reappears for anything newer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismissed_update: Option<String>,
}

fn default_channel() -> String {
    "stable".to_string()
}
fn default_manifest_url() -> String {
    DEFAULT_MANIFEST_URL.to_string()
}
fn default_auth_base_url() -> String {
    "https://fopull.com".to_string()
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
            auth_base_url: default_auth_base_url(),
            dismissed_update: None,
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
        let mut cfg: Self = std::fs::read_to_string(paths.config_file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        // Distribution moved to the public releases repo: configs saved before
        // that carry the old private-repo URL — migrate them (a URL the user
        // customized to anything else is respected).
        if cfg.settings.manifest_url == LEGACY_MANIFEST_URL {
            cfg.settings.manifest_url = DEFAULT_MANIFEST_URL.to_string();
        }
        // A LOCAL manifest path (the dev/testing source) that no longer exists
        // can only ever error — self-heal to the public default instead of
        // showing a dead version list forever.
        if !cfg.settings.manifest_url.starts_with("http")
            && !std::path::Path::new(&cfg.settings.manifest_url).exists()
        {
            cfg.settings.manifest_url = DEFAULT_MANIFEST_URL.to_string();
        }
        // The Phase-1 sign-in server. A NEW DEFAULT DOES NOT FIX THIS on its own:
        // the value is persisted, so the installs that carry the dead host are
        // exactly the ones that have run before — which is everyone who ever
        // signed in. Rewritten on load rather than left for the user to notice,
        // because "502" is not a sentence anybody can act on.
        if cfg.settings.auth_base_url.contains(RETIRED_AUTH_HOST) {
            cfg.settings.auth_base_url = default_auth_base_url();
        }
        cfg
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
    fn the_retired_sign_in_host_is_migrated_off_on_load() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::at(tmp.path());
        std::fs::create_dir_all(&paths.config).unwrap();
        std::fs::write(
            paths.config_file(),
            r#"{"settings":{"auth_base_url":"https://dev-auth.fopull.com"},"projects":[]}"#,
        )
        .unwrap();
        assert_eq!(HubConfig::load(&paths).settings.auth_base_url, "https://fopull.com");

        // Anything the user actually chose is still theirs — a local dev provider
        // is a supported target, and self-healing must not mean overruling.
        std::fs::write(
            paths.config_file(),
            r#"{"settings":{"auth_base_url":"http://localhost:8000"},"projects":[]}"#,
        )
        .unwrap();
        assert_eq!(HubConfig::load(&paths).settings.auth_base_url, "http://localhost:8000");
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
