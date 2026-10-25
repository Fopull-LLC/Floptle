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
    /// Where a launched editor's terminal output goes — see `launch::launch`.
    pub fn logs_dir(&self) -> PathBuf {
        self.data.join("logs")
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
    /// List beta releases beside the stable ones, and offer a newer beta as an update.
    /// Off for a new user: stable releases only.
    #[serde(default)]
    pub beta_updates: bool,
    /// The setting [`beta_updates`](Self::beta_updates) replaced: a `"stable"` | `"beta"`
    /// channel, which showed one channel's releases and hid the other's. Read so a hub.json
    /// from before keeps its choice (see [`HubConfig::load`]); never written back.
    #[serde(default, rename = "channel", skip_serializing)]
    legacy_channel: Option<String>,
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
    /// The anonymous daily install ping (see `ping.rs`). On unless turned off.
    #[serde(default = "default_true")]
    pub install_ping: bool,
    /// Whether the ping's notice has been shown and acknowledged. Nothing is
    /// sent before it has.
    #[serde(default)]
    pub ping_notice_seen: bool,
}

fn default_true() -> bool {
    true
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

impl Settings {
    /// Whether a release on `channel` is listed and offered: stable always, beta when
    /// [`beta_updates`](Self::beta_updates) is on.
    pub fn lists(&self, channel: &str) -> bool {
        channel == "stable" || (self.beta_updates && channel == "beta")
    }

    /// The channel this Hub follows, by its old name — what the install ping reports.
    pub fn channel(&self) -> &'static str {
        if self.beta_updates { "beta" } else { "stable" }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            beta_updates: false,
            legacy_channel: None,
            default_version: None,
            manifest_url: default_manifest_url(),
            projects_dir: None,
            auth_base_url: default_auth_base_url(),
            dismissed_update: None,
            install_ping: true,
            ping_notice_seen: false,
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
        cfg.sort_projects();
        // The channel picker became a toggle. Someone who had picked beta keeps seeing
        // betas; stable, or no choice at all, is the toggle's default of off.
        if let Some(channel) = cfg.settings.legacy_channel.take() {
            cfg.settings.beta_updates |= channel == "beta";
        }
        // Distribution moved to the public releases repo: configs saved before
        // that carry the old private-repo URL — migrate them (a URL the user
        // customized to anything else is respected).
        if cfg.settings.manifest_url == LEGACY_MANIFEST_URL {
            cfg.settings.manifest_url = DEFAULT_MANIFEST_URL.to_string();
        }
        // A local manifest path (the dev/testing source) that no longer exists
        // can only ever error — self-heal to the public default instead of
        // showing a dead version list forever.
        if !cfg.settings.manifest_url.starts_with("http")
            && !std::path::Path::new(&cfg.settings.manifest_url).exists()
        {
            cfg.settings.manifest_url = DEFAULT_MANIFEST_URL.to_string();
        }
        // The Phase-1 sign-in server. A new default does not FIX this on its own:
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

    /// Stamp the project at `path` as opened at `stamp` (see
    /// [`crate::registry::iso8601_utc`]) and move it to the top of the list.
    pub fn mark_opened(&mut self, path: &Path, stamp: String) {
        let Some(idx) = self.projects.iter().position(|p| p.path == path) else { return };
        let mut project = self.projects.remove(idx);
        project.last_opened = Some(stamp);
        // Front first, then the stable sort: two opens in the same second keep
        // the later one on top.
        self.projects.insert(0, project);
        self.sort_projects();
    }

    /// Most recently opened first; never-opened projects keep their order at the end.
    pub fn sort_projects(&mut self) {
        self.projects.sort_by(|a, b| b.last_opened.cmp(&a.last_opened));
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
        assert!(!loaded.settings.beta_updates, "a new user sees stable releases only");
        assert_eq!(loaded.settings.channel(), "stable");
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

    /// **A beta user stays on betas.** hub.json used to carry a `channel` of `"stable"`
    /// or `"beta"`; the toggle that replaced it has to read the old choice, and the file
    /// it writes back must not carry the old field to be misread later.
    #[test]
    fn an_old_beta_channel_turns_beta_updates_on() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::at(tmp.path());
        std::fs::create_dir_all(&paths.config).unwrap();
        std::fs::write(
            paths.config_file(),
            r#"{"settings":{"channel":"beta","default_version":"0.117.0-rc22"},"projects":[]}"#,
        )
        .unwrap();
        let cfg = HubConfig::load(&paths);
        assert!(cfg.settings.beta_updates, "an old beta choice survives the upgrade");
        assert_eq!(cfg.settings.channel(), "beta");
        assert!(cfg.settings.lists("beta") && cfg.settings.lists("stable"));
        assert_eq!(cfg.settings.default_version.as_deref(), Some("0.117.0-rc22"));

        // Saved and read back: the toggle, and nothing of the old field.
        cfg.save(&paths).unwrap();
        let text = std::fs::read_to_string(paths.config_file()).unwrap();
        assert!(!text.contains("\"channel\""), "{text}");
        assert!(HubConfig::load(&paths).settings.beta_updates);

        // An old stable choice is the toggle off.
        std::fs::write(paths.config_file(), r#"{"settings":{"channel":"stable"},"projects":[]}"#).unwrap();
        let cfg = HubConfig::load(&paths);
        assert!(!cfg.settings.beta_updates);
        assert!(cfg.settings.lists("stable") && !cfg.settings.lists("beta"));
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
    fn stamps_are_utc_iso8601() {
        use crate::registry::iso8601_utc;
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso8601_utc(1_789_999_999), "2026-09-21T14:13:19Z");
    }

    #[test]
    fn the_most_recently_opened_project_is_listed_first() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::at(tmp.path());
        let mut cfg = HubConfig::default();
        for name in ["a", "b", "c", "d"] {
            cfg.upsert_project(Project { name: name.into(), path: PathBuf::from(name), engine_version: None, last_opened: None });
        }
        let order = |cfg: &HubConfig| cfg.projects.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join("");
        cfg.mark_opened(Path::new("c"), "2026-09-01T00:00:00Z".into());
        assert_eq!(order(&cfg), "cabd", "never-opened projects keep their order after the opened ones");
        cfg.mark_opened(Path::new("b"), "2026-09-02T00:00:00Z".into());
        assert_eq!(order(&cfg), "bcad");
        // Same second: the later open wins.
        cfg.mark_opened(Path::new("c"), "2026-09-02T00:00:00Z".into());
        assert_eq!(order(&cfg), "cbad");
        // A hub.json written in another order is read back sorted; the
        // never-opened ones keep the file's order.
        cfg.mark_opened(Path::new("c"), "2026-09-03T00:00:00Z".into());
        cfg.projects.reverse();
        assert_eq!(order(&cfg), "dabc");
        cfg.save(&paths).unwrap();
        assert_eq!(order(&HubConfig::load(&paths)), "cbda");
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
