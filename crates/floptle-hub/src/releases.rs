//! Where installable engine versions come from: the release **manifest** and a
//! [`VersionSource`] abstraction over it (the real GitHub-Releases pipeline + a local dev
//! source).
//!
//! The manifest types and the HTTP fetch live in `floptle-dist`, shared with the editor's
//! export templates — a template and an installable engine are the same bundle, so they
//! resolve it the same way. See docs/hub-proposal.md §3–§4.4.

pub use floptle_dist::{Artifact, Manifest, ReleaseInfo, platform_target, version_key};
use std::path::PathBuf;

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

impl VersionSource for GithubReleases {
    fn manifest(&self) -> Result<Manifest, String> {
        floptle_dist::fetch_manifest(&self.manifest_url, self.token.as_deref())
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

    /// The Hub's own view of the shared types still behaves — the parsing and
    /// ordering rules themselves are tested in `floptle-dist`.
    #[test]
    fn a_local_source_reads_and_filters_a_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("releases.json");
        std::fs::write(
            &p,
            r#"{ "schema": 1,
                 "versions": [
                   { "version": "0.3.0", "channel": "stable",
                     "artifacts": { "linux-x86_64": { "url": "u", "sha256": "abc", "size": 10 } } },
                   { "version": "0.4.0-rc1", "channel": "beta", "artifacts": {} } ] }"#,
        )
        .unwrap();
        let src = LocalBuilds { manifest_path: p };
        let m = src.manifest().unwrap();
        assert_eq!(m.on_channel("stable").len(), 1);
        assert_eq!(m.on_channel("beta").len(), 1);
        assert_eq!(m.versions[0].artifacts["linux-x86_64"].sha256, "abc");
    }

    #[test]
    fn a_missing_local_manifest_is_an_error_not_a_panic() {
        let src = LocalBuilds { manifest_path: PathBuf::from("/nonexistent/releases.json") };
        assert!(src.manifest().is_err());
    }
}
