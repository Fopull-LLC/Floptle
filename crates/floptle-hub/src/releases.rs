//! Where installable engine versions come from: the release **manifest**, a
//! [`VersionSource`] abstraction over it (the real GitHub-Releases pipeline + a local dev
//! source), and version ordering. See docs/hub-proposal.md §3–§4.4.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// A downloadable bundle for one (version, platform).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Artifact {
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub size: u64,
}

/// One release: a version and its per-platform artifacts (keyed by [`platform_target`]).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ReleaseInfo {
    pub version: String,
    #[serde(default = "default_channel")]
    pub channel: String,
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub notes_url: String,
    #[serde(default)]
    pub artifacts: BTreeMap<String, Artifact>,
}

impl ReleaseInfo {
    /// The artifact for THIS host's platform, if this release ships one.
    pub fn artifact_here(&self) -> Option<&Artifact> {
        self.artifacts.get(platform_target().as_str())
    }
}

/// The whole `releases.json`.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Manifest {
    #[serde(default)]
    pub schema: u32,
    #[serde(default)]
    pub channels: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub versions: Vec<ReleaseInfo>,
}

impl Manifest {
    pub fn parse(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| format!("bad manifest: {e}"))
    }

    /// Releases on `channel`, newest first.
    pub fn on_channel(&self, channel: &str) -> Vec<ReleaseInfo> {
        let mut v: Vec<ReleaseInfo> =
            self.versions.iter().filter(|r| r.channel == channel).cloned().collect();
        v.sort_by_key(|r| std::cmp::Reverse(version_key(&r.version)));
        v
    }
}

fn default_channel() -> String {
    "stable".to_string()
}

/// The platform target key ("linux-x86_64", "macos-aarch64", "windows-x86_64", …) — matches
/// the artifact keys the release pipeline emits (docs/hub-proposal.md §3.1). `cfg!` is a
/// compile-time constant, so this resolves to this build's platform.
pub fn platform_target() -> String {
    let os = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") { "aarch64" } else { "x86_64" };
    format!("{os}-{arch}")
}

/// One dot-separated pre-release identifier. Per semver, an all-digit identifier compares
/// numerically and sorts BEFORE an alphanumeric one — the derived `Ord` gives `Num < Text`.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Debug)]
pub enum PreId {
    Num(u64),
    Text(String),
}

/// A comparable key for a version string: `(major, minor, patch)` compared numerically
/// (so 0.10 > 0.9, and a missing component is 0), then a stage (0 = pre-release, sorts
/// before 1 = final release of the same base), then the pre-release identifiers compared
/// semver-style (so `rc2` < `rc10`). A fixed-width numeric head keeps the stage/pre tiebreak
/// meaningful regardless of how many components the string has.
pub fn version_key(v: &str) -> (u64, u64, u64, u8, Vec<PreId>) {
    let (base, pre) = match v.split_once('-') {
        Some((b, p)) => (b, Some(p)),
        None => (v, None),
    };
    let mut nums = base.split('.').map(|s| s.trim().parse::<u64>().unwrap_or(0));
    let major = nums.next().unwrap_or(0);
    let minor = nums.next().unwrap_or(0);
    let patch = nums.next().unwrap_or(0);
    let stage = if pre.is_some() { 0u8 } else { 1u8 };
    let ids = pre
        .map(|p| {
            p.split('.')
                .map(|id| id.parse::<u64>().map(PreId::Num).unwrap_or_else(|_| PreId::Text(id.to_string())))
                .collect()
        })
        .unwrap_or_default();
    (major, minor, patch, stage, ids)
}

/// Where the Hub gets the list of installable versions.
pub trait VersionSource {
    fn manifest(&self) -> Result<Manifest, String>;
}

/// The real pipeline: fetch `releases.json` over HTTPS. A private repo needs an auth token
/// (sent as a bearer). Swappable to a public host by pointing `manifest_url` elsewhere.
pub struct GithubReleases {
    pub manifest_url: String,
    pub token: Option<String>,
}

/// True when `url`'s host is GitHub — the ONLY place it's safe to attach a private-repo
/// token (so a manifest/artifact URL pointing elsewhere can't exfiltrate it). Covers the
/// asset CDN (`*.githubusercontent.com`) that release downloads redirect to.
pub fn is_github_host(url: &str) -> bool {
    let after = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).unwrap_or(url);
    let authority = after.split('/').next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("").split(':').next().unwrap_or("");
    host == "github.com" || host.ends_with(".github.com") || host.ends_with(".githubusercontent.com")
}

impl VersionSource for GithubReleases {
    fn manifest(&self) -> Result<Manifest, String> {
        let mut req = ureq::get(&self.manifest_url).set("Accept", "application/octet-stream");
        if let Some(t) = &self.token
            && is_github_host(&self.manifest_url)
        {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        let text = req
            .call()
            .map_err(|e| format!("fetch manifest: {e}"))?
            .into_string()
            .map_err(|e| format!("read manifest: {e}"))?;
        Manifest::parse(&text)
    }
}

/// Dev source: read a manifest from a local file (e.g. produced by the packaging step), so
/// the whole Hub install/launch flow can be tested without cutting a real release.
pub struct LocalBuilds {
    pub manifest_path: PathBuf,
}

impl VersionSource for LocalBuilds {
    fn manifest(&self) -> Result<Manifest, String> {
        let text = std::fs::read_to_string(&self.manifest_path)
            .map_err(|e| format!("read {}: {e}", self.manifest_path.display()))?;
        Manifest::parse(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_target_is_os_dash_arch() {
        let t = platform_target();
        assert!(t.contains('-'));
        assert!(
            ["linux", "macos", "windows"].iter().any(|os| t.starts_with(os)),
            "unexpected target {t}"
        );
    }

    #[test]
    fn version_key_orders_numerically_and_prereleases_first() {
        // Numeric (0.10 > 0.9), pre-releases before the final, DOTTED numeric identifiers
        // compared numerically (rc.2 < rc.10, not lexical), and a short "1.0" == "1.0.0".
        let mut vs = ["0.10.0", "0.2.0", "1.0.0", "1.0.0-rc.10", "1.0.0-rc.2", "0.9.0", "1.0"];
        vs.sort_by(|a, b| version_key(a).cmp(&version_key(b)).then(a.cmp(b)));
        assert_eq!(vs, ["0.2.0", "0.9.0", "0.10.0", "1.0.0-rc.2", "1.0.0-rc.10", "1.0", "1.0.0"]);
    }

    #[test]
    fn manifest_parses_and_filters_by_channel() {
        let json = r#"{
          "schema": 1,
          "channels": { "stable": ["0.3.0"], "beta": ["0.4.0-rc1"] },
          "versions": [
            { "version": "0.3.0", "channel": "stable", "date": "2026-07-04",
              "artifacts": { "linux-x86_64": { "url": "u", "sha256": "abc", "size": 10 } } },
            { "version": "0.2.0", "channel": "stable", "artifacts": {} },
            { "version": "0.4.0-rc1", "channel": "beta", "artifacts": {} }
          ]
        }"#;
        let m = Manifest::parse(json).unwrap();
        assert_eq!(m.schema, 1);
        let stable = m.on_channel("stable");
        assert_eq!(stable.iter().map(|r| r.version.as_str()).collect::<Vec<_>>(), ["0.3.0", "0.2.0"]);
        assert_eq!(m.on_channel("beta").len(), 1);
        let r030 = m.versions.iter().find(|r| r.version == "0.3.0").unwrap();
        assert_eq!(r030.artifacts["linux-x86_64"].sha256, "abc");
    }
}
