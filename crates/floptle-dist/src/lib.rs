//! Shared distribution plumbing.
//!
//! One release bundle — an engine binary plus its `version.json` — is what the
//! Hub installs to run the editor AND what an exported game ships as its player.
//! They are the same artifact, so they share the same code for finding,
//! fetching, and verifying it. See docs/updating-the-hub.md §3–§4.4 and
//! docs/export-builds.md.

mod fetch;
mod manifest;

pub use fetch::{is_github_host, set_executable};
// The two that reach the network and the two that read an archive off a disk.
// A browser has no answer for any of them — and no use for one: a web export
// is served, not downloaded and unpacked by a Hub.
#[cfg(not(target_arch = "wasm32"))]
pub use fetch::{download, fetch_manifest, unpack, verify_sha256};
pub use manifest::{Artifact, Manifest, PreId, ReleaseInfo, version_key};

/// The manifest that lists installable engine versions. Lives on the PUBLIC
/// releases repo — anyone can fetch it and download bundles, no token needed
/// (the engine source stays private; only distribution is public). Swappable
/// to another host without code changes (docs/updating-the-hub.md §3.4).
pub const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/Fopull-LLC/Floptle-releases/releases/download/manifest/releases.json";

/// Every platform the release pipeline publishes a bundle for, in the order a
/// UI should offer them. These are the artifact keys in `releases.json` — and,
/// because an export template IS a release bundle, also the set of platforms a
/// game can be exported for.
pub const PLATFORMS: &[&str] =
    &["linux-x86_64", "windows-x86_64", "macos-aarch64", "macos-x86_64"];

/// The artifact key of the browser build. Not in [`PLATFORMS`]: those are
/// engines the Hub can install, and a browser build is a template an export
/// ships, not a program this machine runs.
pub const WEB_PLATFORM: &str = "web";

/// The file whose presence means a web template is complete: the wasm module
/// wasm-bindgen wrote, under `pkg/`, beside the `.js` glue and the page.
pub const WEB_TEMPLATE_MARKER: &str = "pkg/floptle_web_bg.wasm";

/// The platform target key ("linux-x86_64", "macos-aarch64", "windows-x86_64", …) — matches
/// the artifact keys the release pipeline emits (docs/updating-the-hub.md §3.1). `cfg!` is a
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

/// The executable suffix a binary built for `platform` carries. Derived from the
/// key rather than from `std::env::consts` — the host running the export is not
/// the machine the build is for, which is the entire point.
pub fn exe_suffix_for(platform: &str) -> &'static str {
    if platform.starts_with("windows") { ".exe" } else { "" }
}

/// A human label for a platform key, for pickers and status lines.
pub fn label_for(platform: &str) -> &'static str {
    match platform {
        "linux-x86_64" => "Linux (x86_64)",
        "windows-x86_64" => "Windows (x86_64)",
        "macos-aarch64" => "macOS (Apple Silicon)",
        "macos-x86_64" => "macOS (Intel)",
        WEB_PLATFORM => "Web (browser)",
        _ => "unknown platform",
    }
}

/// The engine binary's name inside a bundle, for `platform`.
pub fn editor_bin_name_for(platform: &str) -> String {
    format!("floptle{}", exe_suffix_for(platform))
}

/// The OS-conventional data dir (`versions/`, `cache/`, `templates/`). `None` if
/// there's no home dir. Shared so the Hub's installs and the editor's export
/// templates land in the same place by construction, not by coincidence.
pub fn data_dir() -> Option<std::path::PathBuf> {
    directories::ProjectDirs::from("com", "Fopull", "Floptle")
        .map(|p| p.data_dir().to_path_buf())
}

/// The OS-conventional config dir (`hub.json`).
pub fn config_dir() -> Option<std::path::PathBuf> {
    directories::ProjectDirs::from("com", "Fopull", "Floptle")
        .map(|p| p.config_dir().to_path_buf())
}

/// Where an unpacked export template for `(version, platform)` lives, under `data`.
/// Versioned because a template MUST match the editor that stamped it — mixing
/// them ships a game whose netcode protocol disagrees with itself.
pub fn template_dir(data: &std::path::Path, version: &str, platform: &str) -> std::path::PathBuf {
    data.join("templates").join(version).join(platform)
}

/// The engine binary inside an unpacked template.
pub fn template_binary(data: &std::path::Path, version: &str, platform: &str) -> std::path::PathBuf {
    template_dir(data, version, platform).join(editor_bin_name_for(platform))
}

/// The PLAYER binary's name inside a bundle, for `platform`.
///
/// A bundle carries two binaries: the editor, which the Hub runs, and the
/// player, which **File ⏵ Export Game…** ships. They are separate programs —
/// the player has no authoring half compiled into it at all — so an export
/// cannot be served by renaming the editor.
pub fn player_bin_name_for(platform: &str) -> String {
    format!("floptle-player{}", exe_suffix_for(platform))
}

/// The file that proves an unpacked template for `platform` is whole — the
/// player binary, or for the web the wasm module. An export refuses a template
/// missing it by name rather than shipping half a build.
pub fn template_marker(data: &std::path::Path, version: &str, platform: &str) -> std::path::PathBuf {
    if platform == WEB_PLATFORM {
        template_dir(data, version, platform).join(WEB_TEMPLATE_MARKER)
    } else {
        template_player_binary(data, version, platform)
    }
}

/// The player binary inside an unpacked template — what an exported build ships.
pub fn template_player_binary(
    data: &std::path::Path,
    version: &str,
    platform: &str,
) -> std::path::PathBuf {
    template_dir(data, version, platform).join(player_bin_name_for(platform))
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

    /// The host always has a bundle published for it, or the Hub could not have
    /// installed this build in the first place.
    #[test]
    fn the_host_platform_is_one_the_pipeline_publishes() {
        assert!(PLATFORMS.contains(&platform_target().as_str()), "{} missing", platform_target());
    }

    /// A template is pinned to BOTH the version and the platform — two editors,
    /// or two targets, must never share a cache slot.
    #[test]
    fn template_paths_are_keyed_on_version_and_platform() {
        let data = std::path::Path::new("/data");
        let a = template_binary(data, "0.11.0", "windows-x86_64");
        assert!(a.ends_with("templates/0.11.0/windows-x86_64/floptle.exe"), "{}", a.display());
        assert_ne!(a, template_binary(data, "0.11.1", "windows-x86_64"), "version must separate");
        assert_ne!(a, template_binary(data, "0.11.0", "linux-x86_64"), "platform must separate");
        assert!(template_binary(data, "0.11.0", "macos-aarch64").ends_with("floptle"));
    }

    #[test]
    fn suffixes_and_names_follow_the_target_not_the_host() {
        assert_eq!(exe_suffix_for("windows-x86_64"), ".exe");
        assert_eq!(exe_suffix_for("linux-x86_64"), "");
        assert_eq!(exe_suffix_for("macos-aarch64"), "");
        assert_eq!(editor_bin_name_for("windows-x86_64"), "floptle.exe");
        assert_eq!(editor_bin_name_for("macos-aarch64"), "floptle");
        for p in PLATFORMS {
            assert_ne!(label_for(p), "unknown platform", "{p} needs a label");
        }
    }
}
