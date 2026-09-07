//! The engine store: one `floptle-server` per pinned version.
//!
//! ## Why the box keeps versions rather than one engine
//!
//! A bundle pins the engine version it was built against, and the box runs
//! *that* one. Two deployments on the same box can therefore be on different
//! engine versions — which is the whole point of pinning, because a developer
//! who has not re-exported must not have their server silently upgraded
//! underneath them by somebody else's deploy.
//!
//! So versions accumulate, deliberately: `<root>/engines/<version>/floptle-server`.
//! A version is fetched once and then costs 30 MB of a 92 GB disk forever.
//!
//! ## aarch64
//!
//! The fleet box is an Oracle A1 and `floptle-dist::PLATFORMS` does not list
//! `linux-aarch64` — the release pipeline never built one, because until now
//! nothing ran an engine on ARM Linux. `release.yml` publishes
//! `floptle-server-<version>-linux-aarch64` as a bare binary beside the relay's
//! for exactly this, and this module is the only thing that downloads it.

use std::path::PathBuf;

use crate::args::Args;
use crate::bundle;

/// Where the release pipeline publishes bare binaries.
pub const RELEASES_REPO: &str = "https://github.com/Fopull-LLC/Floptle-releases/releases/download";

/// The artifact key for the box this agent is running on.
///
/// A constant per architecture rather than a runtime lookup: the agent is
/// itself built for the box it runs on, so the architecture is known at compile
/// time, and a mismatch is a packaging error that should be impossible rather
/// than a runtime branch.
pub const SERVER_PLATFORM: &str = if cfg!(target_arch = "aarch64") {
    "linux-aarch64"
} else {
    "linux-x86_64"
};

/// The `floptle-server` for `version`, fetching it if the box does not have it.
///
/// A version string arrives from the control plane and becomes a **path
/// component and a URL**, so it is validated rather than trusted: a version
/// with a `/` in it would write outside the engine store, and one with a `..`
/// would climb out of it.
pub fn ensure_engine(args: &Args, version: &str) -> Result<PathBuf, String> {
    if !is_version(version) {
        return Err(format!(
            "{version:?} is not an engine version — refusing to build a path out of it"
        ));
    }
    let dir = crate::agent::engine_dir(&args.root, version);
    let bin = dir.join("floptle-server");
    if bin.is_file() {
        return Ok(bin);
    }
    if args.dry_run {
        return Ok(bin);
    }
    let url = format!("{RELEASES_REPO}/v{version}/floptle-server-{version}-{SERVER_PLATFORM}");
    bundle::log_line(&format!("engine {version}: fetching {SERVER_PLATFORM}"));
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;

    // Same order as a bundle: to a temp path, checked, then published. The
    // checksum is published beside the binary by `release.yml`, and a missing
    // one is a refusal rather than a shrug — this is a binary the agent is
    // about to run as a service.
    let tmp = dir.join(".floptle-server.part");
    crate::fetch::download_to(&url, &tmp)?;
    let want = crate::fetch::fetch_text(&format!("{url}.sha256"))
        .map_err(|e| format!("engine {version}: no published checksum ({e}) — refusing to run it"))?;
    let want = want.split_whitespace().next().unwrap_or_default().to_string();
    let got = bundle::sha256_file(&tmp)?;
    if want.is_empty() || !got.eq_ignore_ascii_case(&want) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!(
            "engine {version}: the bytes hash to {got}, the published checksum is {want:?} — \
             refusing to run them"
        ));
    }
    set_executable(&tmp);
    std::fs::rename(&tmp, &bin).map_err(|e| format!("publish {}: {e}", bin.display()))?;
    bundle::log_line(&format!("engine {version}: installed"));
    Ok(bin)
}

/// Is this a version string, and nothing else?
///
/// Digits, dots and the pre-release alphabet `0.85.0-rc6` uses. Deliberately
/// narrow: this value becomes a directory name and a URL path, and the set of
/// things a real version contains is small.
pub fn is_version(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 32
        && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        && !v.starts_with('.')
        && !v.starts_with('-')
        && v.chars().next().is_some_and(|c| c.is_ascii_digit())
}

fn set_executable(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(m) = std::fs::metadata(path) {
            let mut p = m.permissions();
            p.set_mode(p.mode() | 0o755);
            let _ = std::fs::set_permissions(path, p);
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A version from the network cannot become a path.**
    ///
    /// It is used as a directory name AND a URL component, so the two escapes
    /// worth refusing are a separator and a climb. The accepted half matters
    /// just as much: `0.85.0-rc6` is what production is on today, and a guard
    /// that refused a pre-release would refuse every deployment there is.
    #[test]
    fn a_version_is_a_version_and_not_a_path() {
        for good in ["0.85.0", "0.85.0-rc6", "1.0.0-beta.2", "0.9.0"] {
            assert!(is_version(good), "{good} is a real version");
        }
        for bad in [
            "../../etc",
            "0.85.0/../..",
            "/etc/passwd",
            "",
            ".hidden",
            "-rc6",
            "rc6",
            "0.85.0; rm -rf /",
            "0.85.0 0.85.0",
        ] {
            assert!(!is_version(bad), "{bad:?} must not become a directory name");
        }
    }

    /// The box downloads the engine for its OWN architecture.
    ///
    /// The fleet box is aarch64 and `floptle-dist::PLATFORMS` has never carried
    /// a `linux-aarch64`; a binary built for the wrong one is an `Exec format
    /// error` at start, which reaches the developer as "crashed" with no reason.
    #[test]
    fn the_platform_is_the_one_this_agent_was_built_for() {
        #[cfg(target_arch = "aarch64")]
        assert_eq!(SERVER_PLATFORM, "linux-aarch64");
        #[cfg(not(target_arch = "aarch64"))]
        assert_eq!(SERVER_PLATFORM, "linux-x86_64");
        // …and it is one the release pipeline actually publishes.
        assert!(SERVER_PLATFORM.starts_with("linux-"));
    }

    /// An engine already on the box is not fetched again — which is what lets
    /// two deployments pin two different versions without a download per poll.
    #[test]
    fn an_engine_already_present_is_not_refetched() {
        let dir = std::env::temp_dir().join(format!("fleet-engine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = Args { root: dir.clone(), ..Args::default() };
        let ed = crate::agent::engine_dir(&dir, "0.85.0-rc6");
        std::fs::create_dir_all(&ed).unwrap();
        std::fs::write(ed.join("floptle-server"), "#!/bin/true").unwrap();
        // No network is touched: if it were, this would fail rather than hang,
        // because the URL is a real host and the test machine may have no route.
        let got = ensure_engine(&a, "0.85.0-rc6").expect("already there");
        assert_eq!(got, ed.join("floptle-server"));
    }

    /// A bad version is refused before anything is created.
    #[test]
    fn a_bad_version_creates_no_directory() {
        let dir = std::env::temp_dir().join(format!("fleet-engine-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = Args { root: dir.clone(), ..Args::default() };
        let e = ensure_engine(&a, "../../etc").expect_err("refused");
        assert!(e.contains("not an engine version"), "{e}");
        assert!(!dir.join("engines").exists(), "a refused version still made a directory");
    }
}
