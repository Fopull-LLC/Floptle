//! The release manifest (`releases.json`): what versions exist, and where each
//! platform's bundle lives.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A downloadable bundle for one (version, platform).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Artifact {
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub size: u64,
}

/// One release: a version and its per-platform artifacts (keyed by [`super::platform_target`]).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ReleaseInfo {
    pub version: String,
    #[serde(default = "default_channel")]
    pub channel: String,
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub notes_url: String,
    /// The release's name — `"Who's Playing"` — as `docs/releases/vX.Y.Z.md` titles it.
    #[serde(default)]
    pub title: String,
    /// The release notes themselves, in Markdown, lifted verbatim from
    /// `docs/releases/vX.Y.Z.md` by the publish workflow.
    ///
    /// **Carried in the manifest rather than fetched per version.** The alternative is a
    /// request every time somebody clicks a version, which needs a spinner, an error state
    /// and a network connection to read notes for an engine already on the disk. One fetch
    /// at startup buys the whole history offline. It grows — a few KB per release against a
    /// manifest that is already the gate on a 12 MB download — and if that ever stops being
    /// true, the fix is to trim OLD entries here, not to make the common case slower.
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub artifacts: BTreeMap<String, Artifact>,
    /// The **Hub's own** bundles, keyed the same way — how the Hub finds its own updates.
    ///
    /// A separate map rather than a second entry in `artifacts`, because these answer a
    /// different question: `artifacts` is "which engine can I install", this is "am I out
    /// of date". Releases published before v0.21.2 carry none, which is why every reader
    /// treats an absent one as "no update available here" rather than an error.
    #[serde(default)]
    pub hub_artifacts: BTreeMap<String, Artifact>,
    /// Which parts of Floptle this release actually changed: `"engine"`, `"hub"`, or both.
    ///
    /// **One tag builds both binaries**, so every release ships an engine bundle and a Hub
    /// bundle whether or not either of them changed. That is deliberate — a version you can
    /// pin a project to has to be a version you can install and export with — but it means
    /// the version number on its own cannot tell you whether there is a new *engine* in it.
    /// v0.22.1 changed only the Hub, and every reader still saw a new engine to install, a
    /// banner telling them to install it, and an offer to migrate every project onto it.
    ///
    /// **Empty means unknown, and unknown means both.** Every release published before this
    /// field existed carries none, and a reader that read that as "nothing changed" would
    /// quietly stop offering the entire back catalogue.
    #[serde(default)]
    pub changed: Vec<String>,
}

impl ReleaseInfo {
    /// The artifact for THIS host's platform, if this release ships one.
    pub fn artifact_here(&self) -> Option<&Artifact> {
        self.artifacts.get(super::platform_target().as_str())
    }

    /// The **Hub** bundle for this host's platform, if this release ships one.
    pub fn hub_artifact_here(&self) -> Option<&Artifact> {
        self.hub_artifacts.get(super::platform_target().as_str())
    }

    /// The artifact for an arbitrary platform key — what an export template
    /// lookup needs, since a build for another machine is the whole point.
    pub fn artifact_for(&self, platform: &str) -> Option<&Artifact> {
        self.artifacts.get(platform)
    }

    /// Whether this release changed the **engine** — the thing a project pins and runs.
    /// True when [`changed`](Self::changed) is empty, because unknown means both.
    pub fn changes_engine(&self) -> bool {
        self.changed.is_empty() || self.changed.iter().any(|c| c == "engine")
    }

    /// Whether this release changed the **Hub** — the window you install versions from.
    /// True when [`changed`](Self::changed) is empty, because unknown means both.
    pub fn changes_hub(&self) -> bool {
        self.changed.is_empty() || self.changed.iter().any(|c| c == "hub")
    }

    /// A release that shipped a new Hub and the same engine as the version before it —
    /// installable, and not something to move a project onto.
    pub fn is_hub_only(&self) -> bool {
        !self.changed.is_empty() && !self.changes_engine()
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
    /// What the engine is working on and working towards, in Markdown — `docs/news.md`,
    /// embedded by the publish workflow the same way release notes are.
    ///
    /// Release notes answer "what changed in this version". This answers "is this project
    /// alive, and where is it going", which somebody deciding whether to build on an engine
    /// asks first and which no changelog can tell them. It rides the manifest the Hub
    /// already fetches, so it costs no request and works from cache offline. Empty when the
    /// manifest predates this — every reader treats that as "no news to show", never an
    /// error.
    #[serde(default)]
    pub news: String,
}

impl Manifest {
    pub fn parse(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| format!("bad manifest: {e}"))
    }

    /// Releases on `channel`, newest first.
    pub fn on_channel(&self, channel: &str) -> Vec<ReleaseInfo> {
        self.on_channel_refs(channel).into_iter().cloned().collect()
    }

    /// The same list without copying anything.
    ///
    /// Worth having since [`ReleaseInfo::notes`] arrived: an entry used to be a version
    /// string and three URLs, and is now that plus a few KB of Markdown. Anything asking
    /// a *question* of the list — "is there a newer one?", asked several times per frame
    /// by the update checks — must not clone the whole history to answer it.
    pub fn on_channel_refs(&self, channel: &str) -> Vec<&ReleaseInfo> {
        let mut v: Vec<&ReleaseInfo> =
            self.versions.iter().filter(|r| r.channel == channel).collect();
        v.sort_by_key(|r| std::cmp::Reverse(version_key(&r.version)));
        v
    }

    /// The release for an exact version string.
    pub fn release(&self, version: &str) -> Option<&ReleaseInfo> {
        self.versions.iter().find(|r| r.version == version)
    }

    /// Whether moving a project from engine `from` to engine `to` actually lands it on a
    /// different engine — is there any release in `(from, to]` that changed one.
    ///
    /// Not the same question as `to > from`. A Hub-only release bumps the number every
    /// project is measured against without changing a line of the engine, and offering to
    /// migrate a project onto it is offering work with no result. The *range* matters and a
    /// single hop doesn't: 0.21.2 → 0.22.1 crosses 0.22.0, which was an engine release, so
    /// that upgrade is real even though the version it lands on changed only the Hub.
    ///
    /// **Unknown means yes.** A version absent from this manifest — a local build, a
    /// release pulled after the fact, a manifest older than the `changed` field — must not
    /// silently withdraw an upgrade the user can see they're behind on.
    pub fn engine_differs(&self, from: &str, to: &str) -> bool {
        let (lo, hi) = (version_key(from), version_key(to));
        if hi <= lo {
            return false;
        }
        if !self.versions.iter().any(|r| r.version == to) {
            return true;
        }
        self.versions.iter().any(|r| {
            let k = version_key(&r.version);
            k > lo && k <= hi && r.changes_engine()
        })
    }
}

fn default_channel() -> String {
    "stable".to_string()
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
                .map(|id| {
                    id.parse::<u64>().map(PreId::Num).unwrap_or_else(|_| PreId::Text(id.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    (major, minor, patch, stage, ids)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// An export template asks for a platform that is NOT the host — the whole
    /// point of the feature, so it gets its own lookup and its own test.
    #[test]
    fn a_release_yields_an_artifact_for_any_platform_not_just_the_host() {
        let json = r#"{ "versions": [
            { "version": "0.11.0", "channel": "stable", "artifacts": {
                "windows-x86_64": { "url": "w", "sha256": "aa", "size": 1 },
                "macos-aarch64":  { "url": "m", "sha256": "bb", "size": 2 } } } ] }"#;
        let m = Manifest::parse(json).unwrap();
        let r = m.release("0.11.0").expect("release present");
        assert_eq!(r.artifact_for("windows-x86_64").unwrap().sha256, "aa");
        assert_eq!(r.artifact_for("macos-aarch64").unwrap().sha256, "bb");
        assert!(r.artifact_for("linux-x86_64").is_none(), "absent platform is None, not a panic");
        assert!(m.release("9.9.9").is_none());
    }

    /// The v0.22.1 situation, which is what this field exists for: a release that shipped a
    /// new Hub and the same engine, listed as a new engine version to install and to move
    /// every project onto.
    #[test]
    fn a_hub_only_release_is_not_an_engine_release() {
        let json = r#"{ "versions": [
            { "version": "0.21.2", "channel": "stable", "artifacts": {} },
            { "version": "0.22.0", "channel": "stable", "changed": ["engine", "hub"], "artifacts": {} },
            { "version": "0.22.1", "channel": "stable", "changed": ["hub"], "artifacts": {} } ] }"#;
        let m = Manifest::parse(json).unwrap();

        let hub_only = m.release("0.22.1").unwrap();
        assert!(!hub_only.changes_engine());
        assert!(hub_only.changes_hub());
        assert!(hub_only.is_hub_only());

        // Unknown is BOTH, so the whole back catalogue keeps behaving as it always has.
        let old = m.release("0.21.2").unwrap();
        assert!(old.changes_engine() && old.changes_hub());
        assert!(!old.is_hub_only(), "unknown is not a claim that the engine held still");

        // The range, not the endpoint. 0.21.2 → 0.22.1 crosses 0.22.0, so it is a real
        // upgrade even though 0.22.1 itself changed only the Hub…
        assert!(m.engine_differs("0.21.2", "0.22.1"));
        // …and 0.22.0 → 0.22.1 is not, which is the offer that made no sense.
        assert!(!m.engine_differs("0.22.0", "0.22.1"));
        assert!(!m.engine_differs("0.22.1", "0.22.1"));
        assert!(!m.engine_differs("0.22.1", "0.21.2"), "never offer a downgrade");

        // A version this manifest never heard of — a local build — must not quietly
        // withdraw an upgrade the user can see they are behind on.
        assert!(m.engine_differs("0.21.2", "0.23.0"));
    }
}
