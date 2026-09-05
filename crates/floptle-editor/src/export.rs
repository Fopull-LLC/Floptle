//! File ⏵ Export Game… — stamping a runnable build for any platform.
//!
//! # What an export ships
//!
//! **The player, not the editor.** `floptle-player` is a separate binary over
//! the same engine library with the authoring half compiled out (see that
//! crate, and `editor-ui` in this one's manifest): no egui, no dock, no
//! Inspector, no asset browser, no OS file pickers. A build is that binary +
//! the project's assets + a `floptle-game.ron` manifest naming the game.
//!
//! It used to be the editor binary with its chrome hidden behind a
//! `player_mode` flag, which meant every shipped game carried an authoring
//! application it could never open, and anything the editor drew under `if
//! playing` was fixed furniture on somebody's game.
//!
//! # Why there is no compiler here
//!
//! **Nothing about a project is compiled in.** So the binary a build needs is not
//! something to produce — it is something to *fetch*: it is exactly the bundle
//! the release pipeline already publishes for that platform, and the Hub
//! already installs to run the editor.
//!
//! That is the export-template model (Godot's, Unity's). It replaced a
//! `cargo build --target …` that needed the engine source checkout and a C
//! cross-toolchain on the developer's machine — which meant cross-platform
//! export silently could not work from a Hub install, the ordinary way to run
//! the engine, and could never work at all for macOS.
//!
//! A template is pinned to the editor's OWN version. Mixing them would ship a
//! game whose netcode protocol disagrees with the editor that built it.
//!
//! The `cargo` path survives only as [`ExportKind::Template::cross`]: a fallback
//! for a source checkout whose version has no published bundles yet (in
//! practice, engine development between a version bump and its release).

use std::path::{Path, PathBuf};

#[cfg(feature = "editor-ui")]
use crate::Editor;
#[cfg(feature = "editor-ui")]
use floptle_core::time::Instant;
#[cfg(feature = "editor-ui")]
use floptle_script::LogLevel;
#[cfg(feature = "editor-ui")]
use std::sync::mpsc::{Receiver, Sender};

/// The manifest File ⏵ Export Game… writes next to the binary. Its presence
/// turns the binary into a game player; `project` is the assets folder
/// relative to the manifest.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct GameManifest {
    pub(crate) title: String,
    pub(crate) project: String,
    /// Copied from `ProjectConfigDoc::steam` at export time — `project.ron`
    /// itself isn't part of the shipped bundle, so this is the only place a
    /// player's own binary can read its Steam App ID from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) steam: Option<floptle_scene::SteamProjectSettings>,
}

/// A `floptle-game.ron` beside the running binary, if any → (manifest, its dir).
///
/// In a browser "beside the binary" is the root of the bundle the export
/// packed, which is where the export put it.
pub(crate) fn load_game_manifest() -> Option<(GameManifest, PathBuf)> {
    let dir = manifest_dir()?;
    let text = floptle_vfs::read_to_string(dir.join("floptle-game.ron")).ok()?;
    match ron::from_str::<GameManifest>(&text) {
        Ok(m) => Some((m, dir)),
        Err(e) => {
            eprintln!("floptle-game.ron next to the binary is invalid ({e}); starting as editor");
            None
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn manifest_dir() -> Option<PathBuf> {
    Some(std::env::current_exe().ok()?.parent()?.to_path_buf())
}

/// The bundle's root. The export packs `floptle-game.ron` at the top of the
/// bundle, exactly as it writes it beside a native binary.
#[cfg(target_arch = "wasm32")]
fn manifest_dir() -> Option<PathBuf> {
    Some(PathBuf::from("/"))
}

/// How an Export Game… target obtains its PLAYER binary.
#[cfg(feature = "editor-ui")]
pub(crate) enum ExportKind {
    /// The player binary beside the running editor — always this platform.
    SelfBinary,
    /// A published release bundle for `platform`, downloaded and cached.
    /// `cross` is the Rust triple used ONLY as a source-checkout fallback when
    /// this engine version has no published bundle (macOS has none: it cannot
    /// be cross-compiled, which is exactly why templates exist).
    Template { platform: &'static str, cross: Option<&'static str> },
    /// The browser: a published web template (the wasm module, its JS glue
    /// and the page), resolved and cached exactly like a platform bundle
    /// under the `web` artifact key. The build is a folder to serve, not a
    /// binary to run — see [`export_web`]. The source-checkout fallback is
    /// `tools/web/build.sh`, not `cargo build`.
    Web,
}

#[cfg(feature = "editor-ui")]
pub(crate) struct ExportTarget {
    pub(crate) label: &'static str,
    pub(crate) kind: ExportKind,
    pub(crate) exe_suffix: &'static str,
    pub(crate) readme: Option<&'static str>,
}

/// Shipped beside a macOS build: an unsigned binary off the App Store is
/// quarantined, and the failure ("damaged and can't be opened") reads like a
/// broken download rather than a policy.
#[cfg(feature = "editor-ui")]
pub(crate) const MAC_README: &str = "\
Running this build on macOS
===========================

macOS quarantines apps downloaded from the internet that aren't signed by an
Apple-registered developer, and reports them as \"damaged\". The build is fine —
clear the quarantine flag once:

    xattr -dr com.apple.quarantine {exe}
    ./{exe}

Or: right-click the binary in Finder, choose Open, then confirm.
";

/// Shipped beside a web build: how to serve it, and what a browser needs.
pub(crate) const WEB_README: &str = "\
Running this build in a browser
===============================

This folder is a web page. Serve it over HTTP and open index.html — a browser
will not load a game from a file:// URL. Any static host works; for a quick
look on this machine:

    python3 -m http.server 8000      (then open http://localhost:8000/)

To publish on itch.io: zip the CONTENTS of this folder (index.html at the top
of the zip), upload it as an HTML project, and set the viewport size you want.
No special headers are required.

Players need a browser with WebGPU: current Chrome, Edge or Safari. Firefox is
still rolling it out. The page says so, by name, when it is missing.

Saves live in the browser's own storage for this page's address, so they stay
on the machine and the browser they were made in.
";

/// Every target Export Game… offers. "This machine" first (no download); the
/// rest are published bundles. All four platforms are symmetric — the host you
/// export FROM stopped mattering when the compiler left. The browser is last:
/// the same shape, one more artifact key.
#[cfg(feature = "editor-ui")]
pub(crate) const EXPORT_TARGETS: &[ExportTarget] = &[
    ExportTarget {
        label: "This machine",
        kind: ExportKind::SelfBinary,
        exe_suffix: std::env::consts::EXE_SUFFIX,
        readme: None,
    },
    ExportTarget {
        label: "Windows (x86_64)",
        kind: ExportKind::Template {
            platform: "windows-x86_64",
            cross: Some("x86_64-pc-windows-gnu"),
        },
        exe_suffix: ".exe",
        readme: None,
    },
    ExportTarget {
        label: "Linux (x86_64)",
        kind: ExportKind::Template {
            platform: "linux-x86_64",
            cross: Some("x86_64-unknown-linux-gnu"),
        },
        exe_suffix: "",
        readme: None,
    },
    ExportTarget {
        label: "macOS (Apple Silicon)",
        kind: ExportKind::Template { platform: "macos-aarch64", cross: None },
        exe_suffix: "",
        readme: Some(MAC_README),
    },
    ExportTarget {
        label: "macOS (Intel)",
        kind: ExportKind::Template { platform: "macos-x86_64", cross: None },
        exe_suffix: "",
        readme: Some(MAC_README),
    },
    ExportTarget { label: "Web (browser)", kind: ExportKind::Web, exe_suffix: "", readme: Some(WEB_README) },
];

#[cfg(feature = "editor-ui")]
impl ExportTarget {
    /// The release artifact key this target's template comes from, if it has one.
    fn template_key(&self) -> Option<&'static str> {
        match self.kind {
            ExportKind::SelfBinary => None,
            ExportKind::Template { platform, .. } => Some(platform),
            ExportKind::Web => Some(floptle_dist::WEB_PLATFORM),
        }
    }

    /// Stamp the build with a resolved artifact: the player binary for a
    /// native target, the template FOLDER's marker file for the web (what
    /// [`resolve_template`] reports as `Ready` in each case).
    pub(crate) fn stamp(
        &self,
        project_root: &Path,
        out: &Path,
        title: &str,
        artifact: &Path,
    ) -> Result<(String, PathBuf), String> {
        match self.kind {
            ExportKind::Web => {
                // `<template>/pkg/floptle_web_bg.wasm` → `<template>`.
                let template = artifact
                    .parent()
                    .and_then(Path::parent)
                    .ok_or_else(|| format!("{} is not inside a web template", artifact.display()))?;
                export_web(project_root, out, title, template)
            }
            _ => export_game_with(project_root, out, title, artifact, self),
        }
    }
}

/// Progress from the worker that resolves an export template.
#[cfg(feature = "editor-ui")]
pub(crate) enum TemplateProgress {
    Downloading { done: u64, total: u64 },
    Verifying,
    Unpacking,
    Ready(PathBuf),
    /// This engine version has no published bundle for the target — distinct
    /// from [`Self::Failed`] because a source checkout can still build one.
    Unpublished(String),
    Failed(String),
}

/// An export waiting on a background job.
#[cfg(feature = "editor-ui")]
pub(crate) struct ExportJob {
    pub(crate) out_dir: String,
    pub(crate) title: String,
    pub(crate) target: usize,
    pub(crate) started: Instant,
    pub(crate) work: JobWork,
}

#[cfg(feature = "editor-ui")]
pub(crate) enum JobWork {
    /// Fetching a published template (or reading a cached one).
    Template(Receiver<TemplateProgress>),
    /// The source-checkout fallback: a background `cargo build`.
    Cargo { child: std::process::Child, log: PathBuf },
}

/// Resolve the export template for `(version, platform)` to a binary on disk,
/// reporting progress. Runs on a worker thread.
///
/// A cached template is used as-is: it was checksum-verified when it landed, and
/// it is keyed on the exact version, so it cannot go stale without the version
/// changing.
#[cfg(feature = "editor-ui")]
pub(crate) fn resolve_template(
    version: &str,
    platform: &str,
    manifest_url: &str,
    data_dir: &Path,
    tx: &Sender<TemplateProgress>,
) {
    #[cfg(not(target_arch = "wasm32"))]
    let msg = match run_resolve(version, platform, manifest_url, data_dir, tx) {
        Ok(bin) => TemplateProgress::Ready(bin),
        Err(e) => e,
    };
    #[cfg(target_arch = "wasm32")]
    let msg = {
        let _ = (version, platform, manifest_url, data_dir);
        TemplateProgress::Failed(
            "a browser build cannot download an engine bundle — exporting a game is something \
             you do from the desktop editor"
                .into(),
        )
    };
    let _ = tx.send(msg);
}

// Fetching and unpacking a release bundle is the Hub's job on a desktop. A
// browser build has neither the network shape for it nor anything to do with
// the result — `resolve_template` above still compiles, and reports it.
#[cfg(feature = "editor-ui")]
fn run_resolve(
    version: &str,
    platform: &str,
    manifest_url: &str,
    data_dir: &Path,
    tx: &Sender<TemplateProgress>,
) -> Result<PathBuf, TemplateProgress> {
    let cached = floptle_dist::template_marker(data_dir, version, platform);
    if cached.is_file() {
        return Ok(cached);
    }
    let manifest = floptle_dist::fetch_manifest(manifest_url, None)
        .map_err(|e| TemplateProgress::Failed(format!("release manifest: {e}")))?;
    let release = manifest.release(version).ok_or_else(|| {
        TemplateProgress::Unpublished(format!(
            "engine version {version} has no published bundles yet"
        ))
    })?;
    let artifact = release.artifact_for(platform).ok_or_else(|| {
        TemplateProgress::Unpublished(format!(
            "engine version {version} publishes no {platform} bundle"
        ))
    })?;

    let cache = data_dir.join("cache");
    let fname = artifact.url.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("template");
    let archive = cache.join(format!("template-{platform}-{fname}"));
    floptle_dist::download(&artifact.url, None, &archive, artifact.size, &mut |done, total| {
        let _ = tx.send(TemplateProgress::Downloading { done, total });
    })
    .map_err(TemplateProgress::Failed)?;

    let _ = tx.send(TemplateProgress::Verifying);
    floptle_dist::verify_sha256(&archive, &artifact.sha256)
        .map_err(TemplateProgress::Failed)?;

    let _ = tx.send(TemplateProgress::Unpacking);
    // Unpack into staging and require the binary before committing, so an
    // interrupted fetch never leaves a half-populated template that reads as
    // cached — the same discipline the Hub uses for installs.
    let dest = floptle_dist::template_dir(data_dir, version, platform);
    let staging = dest.with_file_name(format!(".staging-{platform}"));
    let _ = std::fs::remove_dir_all(&staging);
    floptle_vfs::create_dir_all(&staging)
        .map_err(|e| TemplateProgress::Failed(format!("template dir: {e}")))?;
    // The PLAYER, not the editor: a bundle carries both, and an export ships
    // the one with no authoring half in it. A bundle from before the two were
    // split has only the editor, and saying so by name beats shipping it. For
    // the web the marker is the wasm module itself.
    let bin_name = if platform == floptle_dist::WEB_PLATFORM {
        floptle_dist::WEB_TEMPLATE_MARKER.to_string()
    } else {
        floptle_dist::player_bin_name_for(platform)
    };
    let staged = (|| {
        floptle_dist::unpack(&archive, &staging)?;
        if !staging.join(&bin_name).is_file() {
            return Err(format!(
                "the {platform} bundle for this engine version contains no {bin_name}, so \
                 there is no player to ship. It predates the editor/player split; publish \
                 a newer release for {platform}."
            ));
        }
        Ok(())
    })();
    if let Err(e) = staged {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(TemplateProgress::Failed(e));
    }
    if dest.exists() {
        let _ = std::fs::remove_dir_all(&dest);
    }
    if let Some(parent) = dest.parent() {
        let _ = floptle_vfs::create_dir_all(parent);
    }
    std::fs::rename(&staging, &dest)
        .map_err(|e| TemplateProgress::Failed(format!("commit template: {e}")))?;
    let _ = floptle_vfs::remove_file(&archive);
    let bin = dest.join(&bin_name);
    floptle_dist::set_executable(&bin);
    Ok(bin)
}

/// The engine source checkout this editor was built from (compiled-in path — a
/// dev machine). `None` if it's gone, which is the normal case for a Hub install.
pub(crate) fn repo_root() -> Option<PathBuf> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?.parent()?;
    floptle_vfs::is_file(repo.join("Cargo.toml")).then(|| repo.to_path_buf())
}

/// The player binary shipped beside this editor.
///
/// An export ships the **player**, and the two binaries travel together: a Hub
/// install unpacks both out of one bundle, and a source checkout builds both
/// into the same `release/`. So "the binary for this machine" is a sibling
/// lookup rather than `current_exe()` — which would ship the editor, which is
/// exactly what this stopped doing.
#[cfg(feature = "editor-ui")]
fn player_beside_editor() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or("this binary has no parent directory")?;
    let bin = dir.join(floptle_dist::player_bin_name_for(&floptle_dist::platform_target()));
    if bin.is_file() {
        return Ok(bin);
    }
    Err(format!(
        "there is no player binary beside this editor ({}), so there is nothing to ship. \
         An exported build is the player, not the editor — reinstall this engine version \
         through the Hub, or in a source checkout run \
         `cargo build --release -p floptle-player`.",
        bin.display()
    ))
}

/// Where a cross-built fallback binary lands.
#[cfg(feature = "editor-ui")]
fn cross_binary_path(triple: Option<&str>, exe_suffix: &str) -> Option<PathBuf> {
    let target_dir =
        std::env::current_exe().ok().and_then(|e| Some(e.parent()?.parent()?.to_path_buf()))?;
    let name = format!("floptle-player{exe_suffix}");
    Some(match triple {
        Some(t) => target_dir.join(t).join("release").join(name),
        None => target_dir.join("release").join(name),
    })
}

/// Spawn the background release `cargo build` used as the source-checkout
/// fallback. Needs the engine source checkout and the rustup + C toolchains.
#[cfg(feature = "editor-ui")]
fn spawn_export_build(triple: Option<&str>, log: &Path) -> Result<std::process::Child, String> {
    let repo = repo_root()
        .ok_or_else(|| "a fallback build needs the engine source checkout".to_string())?;
    let logfile = std::fs::File::create(log).map_err(|e| format!("build log: {e}"))?;
    let mut cmd = std::process::Command::new("cargo");
    cmd.current_dir(repo)
        .args(["build", "--release", "-p", "floptle-player"])
        .stdout(logfile.try_clone().map_err(|e| e.to_string())?)
        .stderr(logfile)
        .stdin(std::process::Stdio::null());
    if let Some(tr) = triple {
        cmd.args(["--target", tr]);
    }
    // Build into the SAME target dir `cross_binary_path` reads (the running
    // editor's). Without this the child cargo used whatever CARGO_TARGET_DIR
    // the environment happened to have — launched differently, the build
    // succeeded in one place while the export looked in another and reported
    // failure over a perfectly good build.
    if let Some(td) = std::env::current_exe()
        .ok()
        .and_then(|e| Some(e.parent()?.parent()?.to_path_buf()))
    {
        cmd.env("CARGO_TARGET_DIR", td);
    }
    if triple == Some("x86_64-pc-windows-gnu") {
        if let Some(bin) = windows_toolchain_bin()? {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let mut paths = vec![bin.clone()];
            paths.extend(std::env::split_paths(&path));
            cmd.env("PATH", std::env::join_paths(paths).map_err(|e| e.to_string())?);
            // llvm-mingw ships compiler-rt/libunwind, but rustc's windows-gnu
            // target links `-lgcc`/`-lgcc_eh` — alias them to libunwind once
            // and point the build at the shim. (A real mingw-w64-gcc on PATH
            // has libgcc and skips all of this.)
            let root = bin.parent().ok_or("llvm-mingw layout")?;
            let shim = root.join("rust-shim");
            let unwind = root.join("x86_64-w64-mingw32/lib/libunwind.a");
            if !shim.join("libgcc.a").is_file() {
                floptle_vfs::create_dir_all(&shim).map_err(|e| format!("shim dir: {e}"))?;
                std::fs::copy(&unwind, shim.join("libgcc.a"))
                    .and_then(|_| std::fs::copy(&unwind, shim.join("libgcc_eh.a")))
                    .map_err(|e| format!("libgcc shim: {e}"))?;
            }
            let mut rustflags = std::env::var("RUSTFLAGS").unwrap_or_default();
            rustflags.push_str(&format!(" -L {}", shim.display()));
            cmd.env("RUSTFLAGS", rustflags.trim());
        }
        cmd.env("CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER", "x86_64-w64-mingw32-gcc");
    }
    cmd.spawn().map_err(|e| format!("spawn cargo: {e}"))
}

/// The mingw cross toolchain for a Windows fallback build: system-wide (PATH) or
/// the user-space llvm-mingw install. Returns the bin dir to prepend to the
/// child's PATH (None = already on PATH).
#[cfg(feature = "editor-ui")]
fn windows_toolchain_bin() -> Result<Option<PathBuf>, String> {
    let cc = "x86_64-w64-mingw32-gcc";
    let on_path = std::env::var_os("PATH").is_some_and(|p| {
        std::env::split_paths(&p)
            .any(|d| d.join(cc).is_file() || d.join(format!("{cc}.exe")).is_file())
    });
    if on_path {
        return Ok(None);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let bin = PathBuf::from(home).join(".local/opt/llvm-mingw/bin");
        if bin.join(cc).is_file() {
            return Ok(Some(bin));
        }
    }
    Err(format!(
        "no Windows cross-toolchain: install llvm-mingw to ~/.local/opt/llvm-mingw \
         (portable, no root) or `{cc}` system-wide (e.g. pacman -S mingw-w64-gcc)"
    ))
}

// --- the bundle ---------------------------------------------------------------

/// Directories the ENGINE writes into a project at runtime, which a shipped
/// build must not carry: `save/` is the player's own save slots
/// (`floptle_script::save`), `replays/` is recorded match logs
/// (`crate::shadow`). Shipping the developer's copies hands every player a
/// pre-populated save and changes what the game does on first launch.
pub(crate) const RUNTIME_DIRS: &[&str] = &["save", "replays"];

/// Whether a project entry should ship. Dot-entries are editor/IDE plumbing
/// (`.floptle` caches, `.luarc.json`); [`RUNTIME_DIRS`] are runtime state — but
/// only at the project ROOT, since a nested folder named `save` is content.
/// Extensions a shipped build can never load, so it never carries them.
///
/// These are **authoring inputs**: the model formats `floptle-convert` turns
/// into a `.glb` at import time (a build loads the `.glb` that came out), the
/// project files of other engines and content tools, and a project's own
/// tooling scripts. None of them has a loader in the engine — a build that
/// ships them is shipping bytes nothing can open.
///
/// It is worth real money on the web, where every byte is a player's wait: a
/// finished first-person game measured **43 MB of these in a 324 MB build**,
/// most of it `.uasset` files that rode along inside bought asset packs.
///
/// Deliberately NOT here: `.meta` (the terrain streamer writes those, beside
/// its `.cfield`/`.tfield`), and anything texty — a script can read its own
/// data files through `assets.getContents`, and guessing which of those are
/// data is not this list's job.
#[cfg(feature = "editor-ui")]
pub(crate) const NEVER_SHIPS: &[&str] = &[
    // Model sources — imported to `.glb`, never loaded at runtime.
    "fbx", "obj", "mtl", "dae", "stl", "ply", "3ds", "max", "ma", "mb",
    // Digital-content-creation project files.
    "blend", "blend1", "blend2", "c4d", "psd", "xcf",
    // Other engines' artifacts, which routinely ride along in asset packs.
    "uasset", "umap", "upk",
    // A project's own tooling.
    "py", "pyc", "pyo",
];

/// Whether a project entry should ship, by NAME (dot-entries, and the runtime
/// dirs at the project root).
#[cfg(feature = "editor-ui")]
fn ships(name: &str, at_root: bool) -> bool {
    !(name.starts_with('.') || (at_root && RUNTIME_DIRS.contains(&name)))
}

/// Whether a FILE should ship, on top of [`ships`]: an authoring input the
/// engine has no loader for does not.
#[cfg(feature = "editor-ui")]
fn ships_file(path: &Path) -> bool {
    !path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| NEVER_SHIPS.contains(&e.to_ascii_lowercase().as_str()))
}

/// What an export deliberately left out, so its message can say so rather
/// than a developer wondering where a folder went.
#[cfg(feature = "editor-ui")]
#[derive(Default)]
pub(crate) struct Skipped {
    pub(crate) files: u64,
    pub(crate) bytes: u64,
}

/// Recursive copy for the export. Returns the number of files copied, and
/// records what [`ships_file`] refused.
#[cfg(feature = "editor-ui")]
fn copy_tree(src: &Path, dst: &Path, at_root: bool, skipped: &mut Skipped) -> std::io::Result<u64> {
    floptle_vfs::create_dir_all(dst)?;
    let mut n = 0;
    for entry in floptle_vfs::read_dir(src)? {
        let name = entry.file_name();
        if !ships(&name.to_string_lossy(), at_root) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if entry.is_dir() {
            n += copy_tree(&from, &to, false, skipped)?;
        } else {
            if !ships_file(&from) {
                skipped.files += 1;
                skipped.bytes += floptle_vfs::size(&from).unwrap_or(0);
                continue;
            }
            std::fs::copy(&from, &to)?;
            n += 1;
        }
    }
    Ok(n)
}

/// Text files that can carry an asset reference.
#[cfg(feature = "editor-ui")]
fn is_texty(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()),
        Some("ron" | "lua" | "flsl" | "json" | "toml")
    )
}

/// What a portability scan found.
#[derive(Default, Debug, PartialEq)]
#[cfg(feature = "editor-ui")]
pub(crate) struct Portability {
    /// Files rewritten from an absolute path into the project to a relative one.
    pub(crate) rewritten: usize,
    /// Absolute paths that point outside the project but whose tail names a
    /// file the build carries — a ref written where the project USED to live —
    /// rewritten to that copy, as `(from, to)`.
    pub(crate) redirected: Vec<(String, String)>,
    /// Absolute paths that point OUTSIDE the project — unfixable here, because
    /// the file they name isn't in the build at all.
    pub(crate) foreign: Vec<String>,
}

/// Make a copied project portable.
///
/// An absolute asset path is taken as written when it exists, so a build
/// carrying one is broken on every machine except the one that exported it —
/// silently, since a missing model just doesn't appear. Rewriting the project
/// root's own prefix is safe by construction: that string can only ever be a
/// path INTO the project.
///
/// A path outside the project whose TAIL is a file the build carries is a ref
/// written where the project used to live (2026-09-05: a browser build staged
/// from a copy shipped 17 files of `/home/…/Forgery/models/…` refs, and every
/// door and NPC was missing in the tab). The player would rescue it the same
/// way (`project::rescue_stranded_root`), but a build should not lean on a
/// rescue for what it can say plainly, so it is rewritten here and reported.
/// A path with no such tail can't be repaired — the file isn't in the build —
/// so it's reported instead.
#[cfg(feature = "editor-ui")]
pub(crate) fn make_portable(shipped: &Path, project_root: &Path) -> Portability {
    let mut out = Portability::default();
    let Some(root) = project_root.to_str() else { return out };
    let prefix = format!("{}{}", root.trim_end_matches(std::path::MAIN_SEPARATOR), std::path::MAIN_SEPARATOR);
    let mut stack = vec![shipped.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = floptle_vfs::read_dir(&dir) else { continue };
        for entry in rd {
            let p = entry.path();
            if entry.is_dir() {
                stack.push(p);
                continue;
            }
            if !is_texty(&p) {
                continue;
            }
            let Ok(mut text) = floptle_vfs::read_to_string(&p) else { continue };
            let mut changed = false;
            if text.contains(&prefix) {
                text = text.replace(&prefix, "");
                changed = true;
                out.rewritten += 1;
            }
            for abs in absolute_refs(&text) {
                match stranded_tail(shipped, &abs) {
                    Some(rel) => {
                        text = text.replace(&format!("\"{abs}\""), &format!("\"{rel}\""));
                        changed = true;
                        if !out.redirected.iter().any(|(from, _)| *from == abs) {
                            out.redirected.push((abs, rel));
                        }
                    }
                    None => {
                        if !out.foreign.contains(&abs) {
                            out.foreign.push(abs);
                        }
                    }
                }
            }
            if changed && floptle_vfs::write(&p, text).is_err() {
                // Counted as rewritten above; an unwritable staging copy is the
                // export's own failure, and the pack that follows reports it.
                out.rewritten = out.rewritten.saturating_sub(1);
            }
        }
    }
    out.redirected.sort();
    out.foreign.sort();
    out
}

/// The project-relative tail of an absolute ref that names a file the shipped
/// copy carries — `/old/place/Forgery/models/door.glb` → `models/door.glb` —
/// or `None`. Forward slashes, as a ref is written.
#[cfg(feature = "editor-ui")]
fn stranded_tail(shipped: &Path, abs: &str) -> Option<String> {
    let found = crate::project::rescue_stranded_root(shipped, abs)?;
    let rel = found.strip_prefix(shipped).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

/// Quoted absolute-looking paths in a text file (`"/x/y"`, `"C:\x"`).
/// Could this absolute string be a file the exporting machine resolved?
///
/// Either its last segment carries an extension (`tree.glb`, `hit.wav`) or the
/// path is really there. A route like `/api/v1.2/session` has a dot in the
/// MIDDLE and none at the end, and points at nothing on disk.
#[cfg(feature = "editor-ui")]
fn looks_like_a_file(abs: &str) -> bool {
    let last = abs.rsplit(['/', '\\']).next().unwrap_or("");
    let has_ext = last
        .rsplit_once('.')
        .is_some_and(|(stem, ext)| !stem.is_empty() && !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphanumeric()));
    has_ext || Path::new(abs).exists()
}

#[cfg(feature = "editor-ui")]
fn absolute_refs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for chunk in text.split('"').skip(1).step_by(2) {
        let is_abs = chunk.starts_with('/')
            || (chunk.len() > 2
                && chunk.as_bytes()[1] == b':'
                && matches!(chunk.as_bytes()[2], b'\\' | b'/'));
        // A bare "/" or a URL is not an asset reference. Nor is an HTTP
        // endpoint path — `"/api/login"` is shaped exactly like an absolute
        // Unix path and was reported as a foreign asset on every export of a
        // game with a login script. What separates a reference from a route:
        // an asset either names a FILE (has an extension on its last segment)
        // or exists on this machine, which a route never does.
        if is_abs
            && chunk.len() > 1
            && !chunk.contains("://")
            && looks_like_a_file(chunk)
            && !out.contains(&chunk.to_string())
        {
            out.push(chunk.to_string());
        }
    }
    out
}

/// The scene a build boots into, resolved the way `scene.load` resolves names:
/// a path relative to the project (`scenes/menu.ron`), or a bare scene name
/// (`menu`). The two conventions used to disagree — `entry_scene` demanded a
/// path while `scene.load` took a name — so a reasonable-looking `"menu"` fell
/// back to `scenes/first.ron` with only an stderr line, and what you playtested
/// was not what shipped.
pub(crate) fn resolve_entry_scene(project_root: &Path, entry: &str) -> Option<PathBuf> {
    let entry = entry.trim();
    if entry.is_empty() {
        return None;
    }
    let direct = project_root.join(entry);
    if floptle_vfs::is_file(&direct) {
        return Some(direct);
    }
    let scenes = project_root.join("scenes");
    [scenes.join(format!("{entry}.ron")), scenes.join(entry)].into_iter().find(|c| floptle_vfs::is_file(c))
}

/// Copy every LINKED package into the shipped project, under the same
/// `packages/<id>/` a copied one occupies. Returns how many were bundled.
///
/// A link is a development convenience — read it where it is being written —
/// and it is exactly the thing that would go missing from a build. Nothing is
/// reported as an error: a link pointing at a folder that has since moved is
/// worth a Console line, not a failed export.
#[cfg(feature = "editor-ui")]
fn ship_linked_packages(proj: &Path, ship_assets: &Path) -> Result<usize, String> {
    let Ok(reg) = floptle_package::Registry::load(proj) else { return Ok(0) };
    let mut n = 0;
    for entry in reg.packages.iter().filter(|e| e.enabled && e.source.is_linked()) {
        let from = entry.root_in(proj);
        if !from.is_dir() {
            continue;
        }
        let to = ship_assets.join(floptle_package::PACKAGES_DIR).join(&entry.id);
        floptle_package::install::copy_dir(&from, &to)
            .map_err(|e| format!("copy linked package `{}`: {e}", entry.id))?;
        n += 1;
    }
    Ok(n)
}

/// Stamp out a runnable build: an engine binary + the project's assets + the
/// `floptle-game.ron` manifest that flips it into player mode.
/// What staging a project into a build folder produced.
#[cfg(feature = "editor-ui")]
struct Staged {
    files: u64,
    linked: usize,
    port: Portability,
    skipped: Skipped,
}

#[cfg(feature = "editor-ui")]
impl Staged {
    /// The status line's tail: what was bundled, rewritten, or left dangling.
    fn tail(&self) -> String {
        let mut msg = String::new();
        if self.skipped.files > 0 {
            msg.push_str(&format!(
                " — left out {} authoring file(s), {:.1} MB the engine has no loader for",
                self.skipped.files,
                self.skipped.bytes as f64 / 1.0e6,
            ));
        }
        if self.linked > 0 {
            msg.push_str(&format!(" — bundled {} linked package(s)", self.linked));
        }
        if self.port.rewritten > 0 {
            msg.push_str(&format!(
                " — made {} file(s) portable (absolute paths into the project)",
                self.port.rewritten
            ));
        }
        if !self.port.redirected.is_empty() {
            msg.push_str(&format!(
                " — redirected {} reference(s) written where the project used to live to the \
                 build's own copy: {}",
                self.port.redirected.len(),
                self.port.redirected.iter().map(|(a, b)| format!("{a} → {b}")).collect::<Vec<_>>().join(", ")
            ));
        }
        if !self.port.foreign.is_empty() {
            msg.push_str(&format!(
                " — ⚠ {} reference(s) point OUTSIDE the project and will be missing on other \
                 machines: {}",
                self.port.foreign.len(),
                self.port.foreign.join(", ")
            ));
        }
        msg
    }
}

/// Create and canonicalise the export folder, and refuse one inside the
/// project (it would copy itself). Returns `(project, out)`, both canonical.
#[cfg(feature = "editor-ui")]
fn prepare_out(project_root: &Path, out: &Path) -> Result<(PathBuf, PathBuf), String> {
    floptle_vfs::create_dir_all(out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let proj = project_root.canonicalize().map_err(|e| format!("project dir: {e}"))?;
    let out_c = out.canonicalize().map_err(|e| format!("export dir: {e}"))?;
    if out_c.starts_with(&proj) {
        return Err("the export folder can't be inside the project (it would copy itself)".into());
    }
    Ok((proj, out_c))
}

/// The part of an export every target shares: check the project, clear the
/// previous copy, copy the project into `<out>/assets` through the ship list,
/// bundle linked packages, make references portable, and write the manifest
/// at `<out>/floptle-game.ron`. `out` exists and is canonical.
#[cfg(feature = "editor-ui")]
fn stage_game(proj: &Path, out_c: &Path, title: &str) -> Result<Staged, String> {
    // A build that can't find its chosen entry scene is dead on arrival — catch
    // it at export time, not on a player's machine.
    let cfg = floptle_scene::load_project(&proj.join("project.ron"));
    if let Some(entry) = cfg.entry_scene.as_deref()
        && !entry.trim().is_empty()
        && resolve_entry_scene(proj, entry).is_none()
    {
        return Err(format!(
            "the project's entry scene ({entry}) doesn't exist — pick one in \
             Edit ⏵ Project Settings"
        ));
    }
    // The build's `assets/` copy is wholly owned by the export: clear the
    // previous one so files deleted from the project don't linger in shipped
    // builds (and a stale FILE named `assets` — the old broken-export
    // artifact — doesn't block the copy).
    let ship_assets = out_c.join("assets");
    if floptle_vfs::is_dir(&ship_assets) {
        std::fs::remove_dir_all(&ship_assets).map_err(|e| format!("clear old assets copy: {e}"))?;
    } else if floptle_vfs::exists(&ship_assets) {
        floptle_vfs::remove_file(&ship_assets).map_err(|e| format!("clear old assets copy: {e}"))?;
    }
    let mut skipped = Skipped::default();
    let files =
        copy_tree(proj, &ship_assets, true, &mut skipped).map_err(|e| format!("copy assets: {e}"))?;
    // A LINKED package is not inside the project, so the copy above missed it —
    // it lives wherever the person writing it keeps it. A build has to carry
    // what it needs, so linked packages are materialised into the shipped
    // `packages/<id>/` here, exactly where a copied one already sits. That is
    // also what makes `pkg://` resolve in a player with no package host: the
    // scheme falls back to `<project>/packages/<id>/`.
    let linked = ship_linked_packages(proj, &ship_assets)?;
    let port = make_portable(&ship_assets, proj);
    let manifest = GameManifest { title: title.to_string(), project: "assets".into(), steam: cfg.steam };
    let text = ron::ser::to_string_pretty(&manifest, ron::ser::PrettyConfig::default())
        .map_err(|e| format!("manifest: {e}"))?;
    floptle_vfs::write(out_c.join("floptle-game.ron"), text).map_err(|e| format!("write manifest: {e}"))?;
    Ok(Staged { files, linked, port, skipped })
}

/// Stamp a native build: the staged project beside the player binary, renamed
/// to the game's title.
#[cfg(feature = "editor-ui")]
pub(crate) fn export_game_with(
    project_root: &Path,
    out: &Path,
    title: &str,
    binary: &Path,
    target: &ExportTarget,
) -> Result<(String, PathBuf), String> {
    let (proj, out_c) = prepare_out(project_root, out)?;
    // Binary name from the title: filesystem-safe, the TARGET's suffix.
    let stem: String = title
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let stem = stem.trim_matches('_');
    let stem = if stem.is_empty() { "game" } else { stem };
    let mut exe_name = format!("{stem}{}", target.exe_suffix);
    // The shipped project folder is literally named `assets` — an exe resolving
    // to that same name (a project rooted at `assets/`, exported for a
    // suffix-less target) would collide with it and corrupt the build.
    if exe_name == "assets" {
        exe_name = "game".into();
    }
    // Everything the game needs ships BEFORE the binary, and the binary ships
    // LAST — a failed export must never leave a runnable-looking exe that,
    // missing its floptle-game.ron, silently boots as the EDITOR.
    let staged = stage_game(&proj, &out_c, title)?;
    if let Some(tpl) = target.readme {
        floptle_vfs::write(out_c.join("README.txt"), tpl.replace("{exe}", &exe_name))
            .map_err(|e| format!("write README: {e}"))?;
    }
    let shipped = out_c.join(&exe_name);
    std::fs::copy(binary, &shipped).map_err(|e| format!("copy binary: {e}"))?;
    // A CI artifact may have lost its executable bit in transit — restore it
    // (only meaningful for unix-family targets; .exe doesn't care).
    floptle_dist::set_executable(&shipped);

    let mut msg = format!("exported {exe_name} + {} asset file(s) to {}", staged.files, out_c.display());
    msg.push_str(&staged.tail());
    Ok((msg, out_c))
}

/// Stamp a browser build: the staged project packed into one bundle the page
/// fetches (`game.flpk`), beside the web template — the page with the game's
/// title in it, the JS glue, and the wasm module.
///
/// The folder is what you serve. Nothing in it is the project's own files by
/// name — everything a player's browser can download is inside the bundle —
/// which is also why the staging folder does not ship: it is packed and gone.
#[cfg(feature = "editor-ui")]
pub(crate) fn export_web(
    project_root: &Path,
    out: &Path,
    title: &str,
    template: &Path,
) -> Result<(String, PathBuf), String> {
    let (proj, out_c) = prepare_out(project_root, out)?;
    // The template first: a folder with no page is not a build.
    const GLUE: &str = "pkg/floptle_web.js";
    for f in ["index.html", GLUE, floptle_dist::WEB_TEMPLATE_MARKER] {
        if !floptle_vfs::is_file(template.join(f)) {
            return Err(format!(
                "the web template at {} has no {f} — rebuild it (tools/web/build.sh) or let the \
                 export download it again",
                template.display()
            ));
        }
    }
    // Staged exactly as a native build is, into a folder that is packed and
    // then removed.
    let staging = out_c.join(".staging");
    let _ = std::fs::remove_dir_all(&staging);
    floptle_vfs::create_dir_all(&staging).map_err(|e| format!("staging dir: {e}"))?;
    let staged = stage_game(&proj, &staging, title)?;
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    collect_files(&staging, &staging, &mut entries)?;
    let packed = floptle_vfs::pack(entries.iter().map(|(p, b)| (p.as_str(), b.as_slice())));
    let bundle_bytes = packed.len();
    floptle_vfs::write(out_c.join("game.flpk"), packed).map_err(|e| format!("write game.flpk: {e}"))?;
    let _ = std::fs::remove_dir_all(&staging);
    // The page, titled; the glue; the module; how to serve it.
    let page = floptle_vfs::read_to_string(template.join("index.html"))
        .map_err(|e| format!("read the template's page: {e}"))?
        .replace("{{TITLE}}", &html_escape(title));
    floptle_vfs::write(out_c.join("index.html"), page).map_err(|e| format!("write index.html: {e}"))?;
    floptle_vfs::create_dir_all(out_c.join("pkg")).map_err(|e| format!("pkg dir: {e}"))?;
    for f in [GLUE, floptle_dist::WEB_TEMPLATE_MARKER] {
        floptle_vfs::copy(template.join(f), out_c.join(f)).map_err(|e| format!("copy {f}: {e}"))?;
    }
    floptle_vfs::write(out_c.join("README.txt"), WEB_README).map_err(|e| format!("write README: {e}"))?;
    let module_bytes = floptle_vfs::read(out_c.join(floptle_dist::WEB_TEMPLATE_MARKER)).map(|b| b.len()).unwrap_or(0);
    let mut msg = format!(
        "exported a web build to {}: game.flpk is {:.1} MB ({} asset file(s)), the engine module {:.1} MB",
        out_c.display(),
        bundle_bytes as f64 / 1.0e6,
        staged.files,
        module_bytes as f64 / 1.0e6,
    );
    // **What is heavy, by kind.** The bundle downloads before the game starts,
    // so its size is a player's wait — and "339 MB" on its own tells nobody
    // which folder to go and look at. Naming the three biggest kinds does.
    let heaviest = heaviest_kinds(&entries, 3);
    if !heaviest.is_empty() {
        msg.push_str(&format!(" — mostly {heaviest}"));
    }
    msg.push_str(&staged.tail());
    msg.push_str(" — serve the folder over HTTP (README.txt says how)");
    Ok((msg, out_c))
}

/// The `n` heaviest file kinds in a bundle, as `"ogg 174.7 MB, png 58.3 MB"`.
///
/// By extension rather than by folder: a project's folder names are its own
/// business, but "the audio is two thirds of this download" is the same
/// sentence in every project.
#[cfg(feature = "editor-ui")]
fn heaviest_kinds(entries: &[(String, Vec<u8>)], n: usize) -> String {
    let mut by_ext: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for (path, bytes) in entries {
        let ext = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_else(|| "no extension".into());
        *by_ext.entry(ext).or_default() += bytes.len() as u64;
    }
    let mut ranked: Vec<(String, u64)> = by_ext.into_iter().collect();
    // Size first; the name only to keep equal sizes in a stable order.
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
        .into_iter()
        .take(n)
        // Under a tenth of a MB the number rounds to 0.0 and says nothing.
        .filter(|(_, b)| *b >= 100_000)
        .map(|(ext, bytes)| format!("{ext} {:.1} MB", bytes as f64 / 1.0e6))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every file under `dir`, as (path relative to `root` with forward slashes,
/// bytes), for the bundle.
#[cfg(feature = "editor-ui")]
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Result<(), String> {
    for entry in floptle_vfs::read_dir(dir).map_err(|e| format!("list {}: {e}", dir.display()))? {
        let p = entry.path();
        if entry.is_dir() {
            collect_files(root, &p, out)?;
        } else {
            let rel = p.strip_prefix(root).map_err(|e| e.to_string())?.to_string_lossy().replace('\\', "/");
            let bytes = floptle_vfs::read(&p).map_err(|e| format!("read {}: {e}", p.display()))?;
            out.push((rel, bytes));
        }
    }
    Ok(())
}

/// A title into the page's HTML, with the five characters that would change
/// its meaning escaped.
#[cfg(feature = "editor-ui")]
fn html_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A web template built from this source checkout by `tools/web/build.sh` —
/// `<target-dir>/web/` — as its marker file, if it is there.
#[cfg(feature = "editor-ui")]
fn web_template_from_checkout() -> Option<PathBuf> {
    repo_root()?;
    let target_dir = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.parent()?.to_path_buf()))?;
    let marker = target_dir.join("web").join(floptle_dist::WEB_TEMPLATE_MARKER);
    floptle_vfs::is_file(&marker).then_some(marker)
}

/// Headless `--export <PROJECT> <OUT> <PLATFORM>`: stamp a build without a
/// window or a GPU. Same code the dialog drives — the template resolution just
/// blocks instead of being polled — so CI and scripts get exactly the editor's
/// behaviour, and this path is what makes the feature verifiable end to end.
///
/// `PLATFORM` is a release artifact key (`windows-x86_64`, `macos-aarch64`, …)
/// or `host` for this machine.
#[cfg(feature = "editor-ui")]
pub(crate) fn headless_export(project: &Path, out: &Path, platform: &str, title: &str) -> i32 {
    let version = crate::distribution_version();
    let target = if platform == "host" {
        &EXPORT_TARGETS[0]
    } else {
        match EXPORT_TARGETS.iter().find(|t| t.template_key() == Some(platform)) {
            Some(t) => t,
            None => {
                eprintln!(
                    "unknown platform {platform:?} — expected `host`, {}, or one of: {}",
                    floptle_dist::WEB_PLATFORM,
                    floptle_dist::PLATFORMS.join(", ")
                );
                return 2;
            }
        }
    };
    let binary = match target.template_key() {
        None => match player_beside_editor() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("export failed: {e}");
                return 1;
            }
        },
        Some(_) if cfg!(debug_assertions) && matches!(target.kind, ExportKind::Web) && web_template_from_checkout().is_some() => {
            // See `begin_export`: a debug editor ships the checkout's own build.
            println!("using the web template tools/web/build.sh built in this checkout");
            web_template_from_checkout().expect("checked")
        }
        Some(platform) => {
            let Some(data) = floptle_dist::data_dir() else {
                eprintln!("export failed: no data directory for the template cache");
                return 1;
            };
            println!("resolving the {} engine template for {version}…", target.label);
            let (tx, rx) = std::sync::mpsc::channel();
            resolve_template(&version, platform, floptle_dist::DEFAULT_MANIFEST_URL, &data, &tx);
            drop(tx);
            let mut bin = None;
            let mut last_pct = u64::MAX;
            for msg in rx {
                match msg {
                    TemplateProgress::Downloading { done, total } => {
                        let pct = (done * 100).checked_div(total).unwrap_or(0);
                        if pct != last_pct && pct % 10 == 0 {
                            println!("  downloading… {pct}%");
                            last_pct = pct;
                        }
                    }
                    TemplateProgress::Verifying => println!("  verifying checksum…"),
                    TemplateProgress::Unpacking => println!("  unpacking…"),
                    TemplateProgress::Ready(p) => bin = Some(p),
                    // An unpublished web template has a second source: this
                    // checkout's own build of it.
                    TemplateProgress::Unpublished(e)
                        if matches!(target.kind, ExportKind::Web) && web_template_from_checkout().is_some() =>
                    {
                        println!("  {e} — using the one tools/web/build.sh built in this checkout");
                        bin = web_template_from_checkout();
                    }
                    TemplateProgress::Unpublished(e) | TemplateProgress::Failed(e) => {
                        eprintln!("export failed: {e}");
                        return 1;
                    }
                }
            }
            match bin {
                Some(b) => b,
                None => {
                    eprintln!("export failed: the template produced no binary");
                    return 1;
                }
            }
        }
    };
    match target.stamp(project, out, title, &binary) {
        Ok((msg, _)) => {
            println!("{msg}");
            0
        }
        Err(e) => {
            eprintln!("export failed: {e}");
            1
        }
    }
}

// --- driving it from the editor -----------------------------------------------

#[cfg(feature = "editor-ui")]
impl Editor {
    /// Export Game… clicked.
    pub(crate) fn begin_export(&mut self, dir: String, target: usize) {
        let dir = self.resolve_export_dir(&dir).display().to_string();
        let title = if self.export_title.trim().is_empty() {
            self.project_root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "game".into())
        } else {
            self.export_title.trim().to_string()
        };
        let target = target.min(EXPORT_TARGETS.len() - 1);
        if self.export_job.is_some() {
            self.export_status = Some("an export is already running…".into());
            return;
        }
        let t = &EXPORT_TARGETS[target];
        match t.kind {
            ExportKind::SelfBinary => {
                // A `cargo run` (debug) editor must not ship ITSELF — a debug
                // binary is huge (~600 MB) and slow. With the source checkout
                // around, build the release binary in the background. A release
                // editor (a Hub install) IS the shipping binary: export directly.
                if cfg!(debug_assertions) && repo_root().is_some() {
                    self.begin_cargo_fallback(None, dir, title, target);
                } else {
                    let result = player_beside_editor().and_then(|exe| {
                        export_game_with(&self.project_root, Path::new(&dir), &title, &exe, t)
                    });
                    self.finish_export(result);
                }
            }
            ExportKind::Template { platform, .. } => {
                self.begin_template_export(platform, dir, title, target)
            }
            ExportKind::Web => {
                // A `cargo run` (debug) editor uses the template this checkout
                // built (tools/web/build.sh) when there is one — the same rule
                // "This machine" follows: what a developer just built is what
                // they mean to ship. A release editor fetches the published one.
                if cfg!(debug_assertions)
                    && let Some(marker) = web_template_from_checkout()
                {
                    let result = t.stamp(&self.project_root, Path::new(&dir), &title, &marker);
                    self.finish_export(result);
                } else {
                    self.begin_template_export(floptle_dist::WEB_PLATFORM, dir, title, target)
                }
            }
        }
    }

    /// Kick off the template worker. A cached template still goes through the
    /// worker so there is exactly one code path (it just finishes immediately).
    fn begin_template_export(
        &mut self,
        platform: &'static str,
        out_dir: String,
        title: String,
        target: usize,
    ) {
        let Some(data) = floptle_dist::data_dir() else {
            self.finish_export(Err("no data directory for the template cache".into()));
            return;
        };
        let version = crate::distribution_version();
        let cached = floptle_vfs::is_file(floptle_dist::template_marker(&data, &version, platform));
        let url = floptle_dist::DEFAULT_MANIFEST_URL.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        let v = version.clone();
        std::thread::spawn(move || resolve_template(&v, platform, &url, &data, &tx));
        self.export_status = Some(if cached {
            format!("📦 stamping the {} build…", EXPORT_TARGETS[target].label)
        } else {
            format!(
                "⬇ fetching the {} engine template for {version} (once — it's cached after this)…",
                EXPORT_TARGETS[target].label
            )
        });
        self.export_job = Some(ExportJob {
            out_dir,
            title,
            target,
            started: Instant::now(),
            work: JobWork::Template(rx),
        });
    }

    /// The source-checkout fallback build (also used when a debug editor exports
    /// "This machine").
    fn begin_cargo_fallback(
        &mut self,
        triple: Option<&'static str>,
        out_dir: String,
        title: String,
        target: usize,
    ) {
        let log = std::env::temp_dir().join("floptle-export-build.log");
        match spawn_export_build(triple, &log) {
            Ok(child) => {
                self.export_status = Some(format!(
                    "🔨 no published template for this engine version — building the {} binary \
                     from source (first build takes minutes; log: {})",
                    EXPORT_TARGETS[target].label,
                    log.display()
                ));
                self.export_job = Some(ExportJob {
                    out_dir,
                    title,
                    target,
                    started: Instant::now(),
                    work: JobWork::Cargo { child, log },
                });
            }
            Err(e) => self.finish_export(Err(e)),
        }
    }

    pub(crate) fn finish_export(&mut self, result: Result<(String, PathBuf), String>) {
        let (level, line) = match result {
            Ok((msg, dir)) => {
                self.export_done = Some(dir);
                let level =
                    if msg.contains('⚠') { LogLevel::Warn } else { LogLevel::Debug };
                (level, format!("✅ {msg}"))
            }
            Err(e) => {
                self.export_done = None;
                (LogLevel::Error, format!("📦 export failed: {e}"))
            }
        };
        self.console.push(level, line.clone(), None);
        self.export_status = Some(line);
    }

    /// Where a typed export folder actually lands: absolute paths as-is;
    /// relative paths resolve against the PROJECT's parent folder (predictable
    /// and next to your work — never the process's working directory, which
    /// depends on how the editor was launched).
    pub(crate) fn resolve_export_dir(&self, dir: &str) -> PathBuf {
        let p = Path::new(dir.trim());
        if p.is_absolute() {
            return p.to_path_buf();
        }
        // A relative project root (the default `assets/`) would make the result
        // CWD-relative after all — pin it to the CWD explicitly so the resolved
        // path we display is the path we actually write.
        let root = if self.project_root.is_absolute() {
            self.project_root.clone()
        } else {
            std::env::current_dir().unwrap_or_default().join(&self.project_root)
        };
        root.parent().map(Path::to_path_buf).unwrap_or(root).join(p)
    }

    /// Once per frame: advance a running export and complete it when its binary
    /// is ready.
    pub(crate) fn poll_export_build(&mut self) {
        let template = match self.export_job.as_ref() {
            None => return,
            Some(j) => matches!(j.work, JobWork::Template(_)),
        };
        let result = if template {
            match self.drain_template() {
                // Still fetching, or the job was replaced by a fallback build.
                None | Some(None) => return,
                Some(Some(r)) => r,
            }
        } else {
            let done = match self.export_job.as_mut().map(|j| &mut j.work) {
                Some(JobWork::Cargo { child, .. }) => !matches!(child.try_wait(), Ok(None)),
                _ => return,
            };
            if !done {
                return;
            }
            match self.reap_cargo() {
                Some(r) => r,
                None => return,
            }
        };
        self.export_job = None;
        self.finish_export(result);
    }

    /// Read everything the template worker has sent. `None` = still working;
    /// `Some(None)` = this job was replaced by a fallback build.
    ///
    /// The job is taken out of `self` for the duration so the receiver isn't
    /// borrowed from the same struct the status line writes into.
    #[allow(clippy::option_option)]
    fn drain_template(&mut self) -> Option<Option<Result<(String, PathBuf), String>>> {
        let job = self.export_job.take()?;
        let JobWork::Template(rx) = &job.work else {
            self.export_job = Some(job);
            return None;
        };
        let label = EXPORT_TARGETS[job.target].label;
        let mut status = None;
        let mut ready = None;
        let mut ended = None;
        let mut pending = false;
        loop {
            match rx.try_recv() {
                Ok(TemplateProgress::Downloading { done, total }) => {
                    let pct = (done * 100).checked_div(total).unwrap_or(0);
                    status = Some(format!(
                        "⬇ fetching the {label} engine template — {pct}% ({:.1}/{:.1} MB)",
                        done as f64 / 1.0e6,
                        total as f64 / 1.0e6,
                    ));
                }
                Ok(TemplateProgress::Verifying) => {
                    status = Some("🔒 verifying the template's checksum…".into());
                }
                Ok(TemplateProgress::Unpacking) => {
                    status = Some("📂 unpacking the template…".into());
                }
                Ok(TemplateProgress::Ready(bin)) => {
                    ready = Some(bin);
                    break;
                }
                Ok(TemplateProgress::Unpublished(why)) => {
                    ended = Some(why);
                    break;
                }
                Ok(TemplateProgress::Failed(e)) => {
                    ended = Some(e);
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    pending = true;
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    ended = Some("the template worker stopped unexpectedly".into());
                    break;
                }
            }
        }
        if let Some(s) = status {
            self.export_status = Some(s);
        }
        if pending {
            self.export_job = Some(job);
            return None;
        }
        if let Some(bin) = ready {
            let t = &EXPORT_TARGETS[job.target];
            let r = t.stamp(&self.project_root, Path::new(&job.out_dir), &job.title, &bin).map(|(m, d)| {
                (format!("{m} (in {:.0} s)", job.started.elapsed().as_secs_f32()), d)
            });
            return Some(Some(r));
        }
        let why = ended.unwrap_or_else(|| "template resolution ended without a result".into());
        // Unpublished web template: a source checkout may have built one.
        if matches!(EXPORT_TARGETS[job.target].kind, ExportKind::Web) && why.contains("no published") {
            let t = &EXPORT_TARGETS[job.target];
            return Some(Some(match web_template_from_checkout() {
                Some(marker) => t.stamp(&self.project_root, Path::new(&job.out_dir), &job.title, &marker),
                None => Err(format!(
                    "{why} — in a source checkout, tools/web/build.sh builds one and the export \
                     uses it"
                )),
            }));
        }
        // Unpublished: a source checkout can still build this target itself.
        if let ExportKind::Template { cross: Some(triple), .. } = EXPORT_TARGETS[job.target].kind
            && repo_root().is_some()
            && why.contains("no published")
        {
            self.begin_cargo_fallback(Some(triple), job.out_dir, job.title, job.target);
            return Some(None);
        }
        Some(Some(Err(why)))
    }

    /// Reap a finished fallback build and stamp its binary.
    fn reap_cargo(&mut self) -> Option<Result<(String, PathBuf), String>> {
        let mut job = self.export_job.take()?;
        let JobWork::Cargo { child, log } = &mut job.work else { return None };
        let status = child.wait();
        let t = &EXPORT_TARGETS[job.target];
        let triple = match t.kind {
            ExportKind::Template { cross, .. } => cross,
            ExportKind::SelfBinary | ExportKind::Web => None,
        };
        let result = match status {
            Ok(s) if s.success() => match cross_binary_path(triple, t.exe_suffix) {
                Some(bin) if bin.is_file() => export_game_with(
                    &self.project_root,
                    Path::new(&job.out_dir),
                    &job.title,
                    &bin,
                    t,
                )
                .map(|(m, d)| {
                    (format!("{m} (built in {:.0} s)", job.started.elapsed().as_secs_f32()), d)
                }),
                Some(bin) => Err(format!(
                    "the build succeeded but its binary wasn't at {} — rebuild, or report this",
                    bin.display()
                )),
                None => Err("the build succeeded but its binary wasn't found".into()),
            },
            Ok(s) => Err(format!(
                "the {} build failed (exit {}) — full log: {}",
                t.label,
                s.code().map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
                log.display()
            )),
            Err(e) => Err(format!("build wait: {e}")),
        };
        Some(result)
    }
}

#[cfg(all(test, feature = "editor-ui"))]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("floptle-export-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        floptle_vfs::create_dir_all(&d).unwrap();
        d
    }

    fn target(label: &str) -> &'static ExportTarget {
        EXPORT_TARGETS.iter().find(|t| t.label == label).expect("target exists")
    }

    /// A typed export folder resolves PREDICTABLY: absolute stays put, relative
    /// lands next to the project (its parent) — never the process CWD, which
    /// depends on how the editor was launched.
    #[test]
    fn export_dir_resolves_against_the_project_parent() {
        let mut ed = Editor { project_root: PathBuf::from("/repo/assets"), ..Default::default() };
        assert_eq!(ed.resolve_export_dir("builds"), PathBuf::from("/repo/builds"));
        assert_eq!(ed.resolve_export_dir("/abs/dist"), PathBuf::from("/abs/dist"));
        ed.project_root = PathBuf::from("/");
        assert_eq!(ed.resolve_export_dir("b"), PathBuf::from("/b"));
    }

    /// **A build ships nothing it cannot open.** The model sources an import
    /// consumed, another engine's asset files, a project's own tooling — all
    /// of it is weight, and on the web it is a player's wait. The `.glb` that
    /// came OUT of the import ships; the `.fbx` that went in does not.
    #[test]
    fn authoring_inputs_do_not_ship_but_what_they_produced_does() {
        let proj = temp("strip-proj");
        floptle_vfs::create_dir_all(proj.join("models/pack")).unwrap();
        floptle_vfs::write(proj.join("project.ron"), "()").unwrap();
        // What a build loads.
        floptle_vfs::write(proj.join("models/tree.glb"), vec![b'g'; 400]).unwrap();
        // What it does not: the import's input, a bought pack's leftovers, tooling.
        floptle_vfs::write(proj.join("models/tree.fbx"), vec![b'f'; 5000]).unwrap();
        floptle_vfs::write(proj.join("models/tree.obj"), vec![b'o'; 100]).unwrap();
        floptle_vfs::write(proj.join("models/tree.mtl"), vec![b'm'; 100]).unwrap();
        floptle_vfs::write(proj.join("models/pack/Rock.uasset"), vec![b'u'; 3000]).unwrap();
        floptle_vfs::write(proj.join("models/pack/scene.blend"), vec![b'b'; 900]).unwrap();
        floptle_vfs::write(proj.join("models/pack/build.py"), b"# tool").unwrap();
        // Extensions that LOOK like tooling but are the engine's own, or a
        // game's data — these must survive.
        floptle_vfs::write(proj.join("models/chunk.meta"), b"terrain").unwrap();
        floptle_vfs::write(proj.join("models/notes.txt"), b"read by a script").unwrap();
        let out = temp("strip-out");

        let me = std::env::current_exe().unwrap();
        let (msg, _) = export_game_with(&proj, &out, "Strip", &me, &EXPORT_TARGETS[0]).expect("export");
        let ship = out.join("assets");
        assert!(floptle_vfs::is_file(ship.join("models/tree.glb")), "the imported model must ship");
        assert!(floptle_vfs::is_file(ship.join("models/chunk.meta")), ".meta is the terrain streamer's");
        assert!(floptle_vfs::is_file(ship.join("models/notes.txt")), "text may be a game's own data");
        for gone in ["models/tree.fbx", "models/tree.obj", "models/tree.mtl", "models/pack/Rock.uasset", "models/pack/scene.blend", "models/pack/build.py"] {
            assert!(!floptle_vfs::exists(ship.join(gone)), "{gone} must not ship");
        }
        // And it SAYS so, with the weight — silence would read as a lost folder.
        assert!(msg.contains("left out 6 authoring file(s)"), "{msg}");
        assert!(msg.contains("0.0 MB") || msg.contains("MB the engine has no loader for"), "{msg}");
        for d in [proj, out] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// A web export is a page, a bundle and the module — and the bundle holds
    /// the same staged project a native build ships as a folder, dot-entries
    /// skipped, manifest at its root, so the player in a tab boots the way the
    /// player on a desktop does.
    #[test]
    fn a_web_export_is_a_page_a_bundle_and_the_module() {
        let proj = temp("web-proj");
        floptle_vfs::create_dir_all(proj.join("scenes")).unwrap();
        floptle_vfs::write(proj.join("project.ron"), "()").unwrap();
        floptle_vfs::write(proj.join("scenes/first.ron"), "(nodes: [])").unwrap();
        floptle_vfs::create_dir_all(proj.join(".floptle")).unwrap();
        floptle_vfs::write(proj.join(".floptle/cache.bin"), "x").unwrap();
        // A template, as tools/web/build.sh lays one out.
        let tpl = temp("web-tpl");
        floptle_vfs::create_dir_all(tpl.join("pkg")).unwrap();
        floptle_vfs::write(tpl.join("index.html"), "<title>{{TITLE}}</title><h1>{{TITLE}}</h1>").unwrap();
        floptle_vfs::write(tpl.join("pkg/floptle_web.js"), "// glue").unwrap();
        floptle_vfs::write(tpl.join(floptle_dist::WEB_TEMPLATE_MARKER), [0u8, 0x61, 0x73, 0x6d]).unwrap();
        let out = temp("web-out");

        let web = target("Web (browser)");
        let marker = tpl.join(floptle_dist::WEB_TEMPLATE_MARKER);
        let (msg, done) = web.stamp(&proj, &out, "Tom & Jerry's <Game>", &marker).expect("web export succeeds");
        assert_eq!(done, out.canonicalize().unwrap());
        assert!(msg.contains("2 asset file(s)"), "dot-entries must be skipped: {msg}");
        // The page carries the title, escaped, and nothing of the staging folder is left.
        let page = floptle_vfs::read_to_string(out.join("index.html")).unwrap();
        assert!(page.contains("Tom &amp; Jerry&#39;s &lt;Game&gt;"), "{page}");
        assert!(!page.contains("{{TITLE}}"));
        assert!(!floptle_vfs::exists(out.join(".staging")), "the staging folder is packed and gone");
        assert!(!floptle_vfs::exists(out.join("assets")), "a web build ships no loose project folder");
        assert!(floptle_vfs::is_file(out.join("pkg/floptle_web.js")));
        assert!(floptle_vfs::is_file(out.join(floptle_dist::WEB_TEMPLATE_MARKER)));
        assert!(floptle_vfs::is_file(out.join("README.txt")));
        // The bundle is the staged project: the manifest at its root, the
        // project under `assets/`, the editor's cache not in it.
        let bundle = floptle_vfs::Bundle::parse(floptle_vfs::read(out.join("game.flpk")).unwrap()).unwrap();
        let paths: Vec<&str> = bundle.paths().collect();
        assert_eq!(paths, vec!["assets/project.ron", "assets/scenes/first.ron", "floptle-game.ron"], "{paths:?}");
        let manifest: GameManifest =
            ron::from_str(std::str::from_utf8(bundle.get("floptle-game.ron").unwrap()).unwrap()).unwrap();
        assert_eq!(manifest.title, "Tom & Jerry's <Game>");
        assert_eq!(manifest.project, "assets");
        assert_eq!(bundle.get("assets/scenes/first.ron"), Some(&b"(nodes: [])"[..]));

        // A template with no module is refused by name, before anything is written.
        let broken = temp("web-tpl-broken");
        floptle_vfs::write(broken.join("index.html"), "x").unwrap();
        let out2 = temp("web-out2");
        let err = web.stamp(&proj, &out2, "x", &broken.join(floptle_dist::WEB_TEMPLATE_MARKER)).unwrap_err();
        assert!(err.contains("pkg/floptle_web.js") || err.contains("has no"), "{err}");
        assert!(!floptle_vfs::exists(out2.join("game.flpk")));
        for d in [proj, tpl, out, broken, out2] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// Export = binary + assets (dot-entries skipped) + a manifest that parses
    /// back and points at the copied project.
    #[test]
    fn export_stamps_a_runnable_build() {
        let proj = temp("proj");
        floptle_vfs::create_dir_all(proj.join("scenes")).unwrap();
        floptle_vfs::write(proj.join("project.ron"), "()").unwrap();
        floptle_vfs::write(proj.join("scenes/first.ron"), "()").unwrap();
        floptle_vfs::create_dir_all(proj.join(".floptle")).unwrap();
        floptle_vfs::write(proj.join(".floptle/cache.bin"), "x").unwrap();
        floptle_vfs::write(proj.join(".luarc.json"), "{}").unwrap();
        let out = temp("out");

        let me = std::env::current_exe().unwrap();
        let (msg, done_dir) =
            export_game_with(&proj, &out, "My Cool Game!", &me, &EXPORT_TARGETS[0])
                .expect("export succeeds");
        assert!(msg.contains("2 asset file(s)"), "dot-entries must be skipped: {msg}");
        assert!(done_dir.is_dir(), "the success result carries the build folder");
        assert!(out.join("assets/project.ron").is_file());
        assert!(out.join("assets/scenes/first.ron").is_file());
        assert!(!out.join("assets/.floptle").exists(), "editor cache must not ship");
        let exe = format!("My_Cool_Game{}", std::env::consts::EXE_SUFFIX);
        assert!(out.join(&exe).is_file(), "missing {exe}");
        let manifest: GameManifest =
            ron::from_str(&floptle_vfs::read_to_string(out.join("floptle-game.ron")).unwrap())
                .expect("manifest parses");
        assert_eq!(manifest.title, "My Cool Game!");
        assert_eq!(manifest.project, "assets");

        // Exporting INTO the project is refused (it would copy itself).
        let inside = proj.join("build");
        assert!(export_game_with(&proj, &inside, "x", &me, &EXPORT_TARGETS[0]).is_err());

        // A macOS-target export ships the Gatekeeper README, {exe} filled in.
        let out2 = temp("out-mac");
        export_game_with(&proj, &out2, "Sea Game", &me, target("macOS (Apple Silicon)"))
            .expect("mac export");
        let readme = floptle_vfs::read_to_string(out2.join("README.txt")).unwrap();
        assert!(readme.contains("./Sea_Game"), "README names the actual binary: {readme}");
        assert!(readme.contains("com.apple.quarantine"));

        for d in [&proj, &out, &out2] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// `ProjectConfigDoc::steam` is copied into the exported manifest — the
    /// player's own binary has no other way to learn its Steamworks App ID,
    /// since `project.ron` itself isn't part of the shipped bundle.
    #[test]
    fn export_carries_the_steam_app_id_into_the_manifest() {
        let proj = temp("proj-steam");
        floptle_vfs::write(proj.join("project.ron"), "(steam: Some((app_id: 480)))").unwrap();
        let out = temp("out-steam");

        let me = std::env::current_exe().unwrap();
        export_game_with(&proj, &out, "Steam Game", &me, &EXPORT_TARGETS[0])
            .expect("export succeeds");
        let manifest: GameManifest =
            ron::from_str(&floptle_vfs::read_to_string(out.join("floptle-game.ron")).unwrap())
                .expect("manifest parses");
        assert_eq!(manifest.steam.map(|s| s.app_id), Some(480));

        for d in [&proj, &out] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// The trap behind "the build opens the editor": a project rooted at
    /// `assets/` exported with the default title on a suffix-less target named
    /// the exe `assets` — colliding with the shipped assets FOLDER. The exe must
    /// dodge the reserved name, and the binary must ship LAST so a failed export
    /// never leaves anything runnable.
    #[test]
    fn export_never_collides_the_exe_with_the_assets_folder() {
        let proj = temp("proj-collide");
        floptle_vfs::write(proj.join("project.ron"), "()").unwrap();
        let out = temp("out-collide");
        floptle_vfs::write(out.join("assets"), "old broken binary").unwrap();

        let me = std::env::current_exe().unwrap();
        let bare = ExportTarget {
            label: "test",
            kind: ExportKind::SelfBinary,
            exe_suffix: "", // suffix-less target = the collision case
            readme: None,
        };
        export_game_with(&proj, &out, "assets", &me, &bare).expect("collision export succeeds");
        assert!(out.join("assets").is_dir(), "assets must be the project folder, not the exe");
        assert!(out.join("game").is_file(), "the exe dodges the reserved name");
        assert!(out.join("floptle-game.ron").is_file(), "the build is a GAME (manifest present)");

        let out2 = temp("out-nobin");
        let err = export_game_with(&proj, &out2, "Cool", &me.join("nope"), &bare)
            .expect_err("bogus binary must fail");
        assert!(err.contains("copy binary"), "fails at the binary step: {err}");
        assert!(!out2.join("Cool").exists(), "no runnable exe after a failed export");

        for d in [&proj, &out, &out2] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// Every platform the release pipeline publishes is offerable, and each
    /// carries the suffix of the TARGET rather than of the host.
    #[test]
    fn every_published_platform_is_an_export_target() {
        for p in floptle_dist::PLATFORMS {
            let t = EXPORT_TARGETS
                .iter()
                .find(|t| matches!(t.kind, ExportKind::Template { platform, .. } if platform == *p))
                .unwrap_or_else(|| panic!("no export target for {p}"));
            assert_eq!(t.exe_suffix, floptle_dist::exe_suffix_for(p), "{p} suffix");
        }
        // macOS has no cross fallback on purpose: it cannot be cross-compiled,
        // which is precisely why templates exist.
        for t in EXPORT_TARGETS {
            if let ExportKind::Template { platform, cross } = t.kind
                && platform.starts_with("macos")
            {
                assert!(cross.is_none(), "{platform} must not claim a cross triple");
            }
        }
    }

    /// The engine writes `save/` and `replays/` into a project at runtime.
    /// Shipping the developer's copies hands every player a pre-populated save —
    /// in the field this made a build boot straight into a match instead of its
    /// menu. Only at the ROOT: a nested `save/` folder is content.
    #[test]
    fn runtime_state_does_not_ship_but_nested_folders_do() {
        let proj = temp("proj-runtime");
        floptle_vfs::write(proj.join("project.ron"), "()").unwrap();
        floptle_vfs::create_dir_all(proj.join("save")).unwrap();
        floptle_vfs::write(proj.join("save/main.ron"), "{}").unwrap();
        floptle_vfs::create_dir_all(proj.join("replays")).unwrap();
        floptle_vfs::write(proj.join("replays/r1.log"), "x").unwrap();
        floptle_vfs::create_dir_all(proj.join("scenes/save")).unwrap();
        floptle_vfs::write(proj.join("scenes/save/level.ron"), "()").unwrap();
        let out = temp("out-runtime");

        let me = std::env::current_exe().unwrap();
        export_game_with(&proj, &out, "G", &me, &EXPORT_TARGETS[0]).expect("export");
        assert!(!out.join("assets/save").exists(), "the developer's saves must not ship");
        assert!(!out.join("assets/replays").exists(), "recorded replays must not ship");
        assert!(out.join("assets/scenes/save/level.ron").is_file(), "nested `save` is content");

        for d in [&proj, &out] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// An absolute asset path resolves as-is with no rescue, so a build carrying
    /// one is broken on every machine but the one that exported it — silently,
    /// because a missing model simply doesn't appear. Paths INTO the project are
    /// rewritten; paths outside it can't be (the file isn't in the build) and are
    /// reported instead.
    #[test]
    fn absolute_asset_paths_are_made_relative_and_foreign_ones_are_reported() {
        let proj = temp("proj-abs");
        floptle_vfs::create_dir_all(proj.join("models")).unwrap();
        floptle_vfs::write(proj.join("models/hero.glb"), "glb").unwrap();
        floptle_vfs::write(proj.join("project.ron"), "()").unwrap();
        let root = proj.canonicalize().unwrap();
        floptle_vfs::write(
            proj.join("hero.anim.ron"),
            format!("(source: \"{}/models/hero.glb\", clip: \"idle\")", root.display()),
        )
        .unwrap();
        floptle_vfs::write(
            proj.join("stage.ron"),
            "(mesh: \"/elsewhere/on/disk/tree.glb\", url: \"https://example.com/x\", \
             endpoint: \"/api/login\", version: \"/api/v1.2/session\")",
        )
        .unwrap();
        // A ref written where the project USED to live (a copy on another disk,
        // a Windows machine): outside this root, but its tail is in the build.
        floptle_vfs::create_dir_all(proj.join("scenes")).unwrap();
        floptle_vfs::write(
            proj.join("scenes/hall.ron"),
            "(a: \"/old/disk/proj-abs/models/hero.glb\", b: \"C:\\\\Users\\\\ty\\\\proj-abs\\\\models\\\\hero.glb\")",
        )
        .unwrap();
        let out = temp("out-abs");

        let me = std::env::current_exe().unwrap();
        let (msg, _) = export_game_with(&proj, &out, "G", &me, &EXPORT_TARGETS[0]).expect("export");

        let anim = floptle_vfs::read_to_string(out.join("assets/hero.anim.ron")).unwrap();
        assert!(anim.contains("\"models/hero.glb\""), "rewritten to project-relative: {anim}");
        assert!(!anim.contains(&root.display().to_string()), "no absolute path survives: {anim}");
        assert!(msg.contains("portable"), "the report mentions the rewrite: {msg}");

        assert!(msg.contains("OUTSIDE the project"), "foreign refs are reported: {msg}");
        assert!(msg.contains("/elsewhere/on/disk/tree.glb"), "and named: {msg}");
        let hall = floptle_vfs::read_to_string(out.join("assets/scenes/hall.ron")).unwrap();
        assert_eq!(
            hall, "(a: \"models/hero.glb\", b: \"models/hero.glb\")",
            "a stranded-root ref is redirected to the build's copy, Windows spelling included"
        );
        assert!(msg.contains("redirected 2 reference(s)"), "and the report says so: {msg}");
        let (_, outside) = msg.split_once("OUTSIDE the project").expect("the foreign list");
        assert!(!outside.contains("proj-abs"), "a redirected ref is not ALSO foreign: {msg}");
        assert!(!msg.contains("https://example.com"), "a URL is not an asset path: {msg}");
        // An HTTP endpoint path looks like an absolute Unix path and is not one:
        // nothing on this machine is at `/api/login`, and it names no file.
        // A real game's `web_login.lua` was reported as four foreign refs on
        // every export because of exactly this.
        assert!(!msg.contains("/api/login"), "an endpoint path is not an asset path: {msg}");
        assert!(!msg.contains("/api/v1.2/session"), "nor is one with a dot mid-path: {msg}");

        for d in [&proj, &out] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// `entry_scene` and `scene.load` used to disagree — a path vs a bare name —
    /// so `"menu"` silently fell back to `scenes/first.ron` and shipped the wrong
    /// scene. Both spellings must resolve, and an unresolvable one must fail the
    /// export rather than ship a build that boots somewhere else.
    #[test]
    fn an_entry_scene_resolves_as_a_path_or_a_bare_name_and_fails_loudly_otherwise() {
        let proj = temp("proj-entry");
        floptle_vfs::create_dir_all(proj.join("scenes")).unwrap();
        floptle_vfs::write(proj.join("scenes/menu.ron"), "()").unwrap();
        floptle_vfs::write(proj.join("scenes/first.ron"), "()").unwrap();

        let by_path = resolve_entry_scene(&proj, "scenes/menu.ron").expect("path form");
        let by_name = resolve_entry_scene(&proj, "menu").expect("bare-name form, like scene.load");
        assert_eq!(by_path, by_name, "both spellings name the SAME scene");
        assert!(by_name.ends_with("menu.ron"), "and it is the menu, not the fallback");
        assert!(resolve_entry_scene(&proj, "nope").is_none());
        assert!(resolve_entry_scene(&proj, "  ").is_none());

        // Written through the real serializer: `load_project` falls back to
        // defaults on ANY parse error, so a hand-rolled fixture would silently
        // test nothing at all.
        let write_entry = |entry: &str| {
            let cfg = floptle_scene::ProjectConfigDoc {
                entry_scene: Some(entry.to_string()),
                ..Default::default()
            };
            floptle_scene::save_project(&cfg, &proj.join("project.ron")).unwrap();
            let back = floptle_scene::load_project(&proj.join("project.ron"));
            assert_eq!(back.entry_scene.as_deref(), Some(entry), "fixture must round-trip");
        };

        let me = std::env::current_exe().unwrap();
        let out = temp("out-entry");
        write_entry("menu");
        export_game_with(&proj, &out, "G", &me, &EXPORT_TARGETS[0]).expect("bare name exports");

        write_entry("ghost");
        let err = export_game_with(&proj, &out, "G", &me, &EXPORT_TARGETS[0])
            .expect_err("a missing entry scene must fail the export");
        assert!(err.contains("entry scene"), "{err}");

        for d in [&proj, &out] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    /// **A bundle with no player cannot stamp a build, and says so.**
    ///
    /// The failure this guards is not a crash: before the editor and the player
    /// were split, an export copied the EDITOR and every shipped game carried an
    /// authoring application it could never open. A bundle published before the
    /// split still contains exactly that binary, and the tempting thing for the
    /// resolver to do is take it. Refusing by name is the whole point.
    #[test]
    fn a_bundle_without_a_player_is_refused_by_name() {
        use sha2::{Digest, Sha256};
        let data = temp("tpl-no-player");
        let archive = data.join("floptle-9.9.9-linux-x86_64.tar.gz");
        {
            let gz = flate2::write::GzEncoder::new(
                std::fs::File::create(&archive).unwrap(),
                flate2::Compression::default(),
            );
            let mut tar = tar::Builder::new(gz);
            // The editor ONLY — a pre-split bundle.
            let payload = b"#!/bin/sh\necho editor\n";
            let mut h = tar::Header::new_gnu();
            h.set_size(payload.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            tar.append_data(&mut h, "floptle", &payload[..]).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let sha: String = {
            let mut h = Sha256::new();
            h.update(floptle_vfs::read(&archive).unwrap());
            h.finalize().iter().map(|b| format!("{b:02x}")).collect()
        };
        let manifest = data.join("releases.json");
        floptle_vfs::write(
            &manifest,
            format!(
                r#"{{ "versions": [ {{ "version": "9.9.9", "channel": "stable", "artifacts": {{
                     "linux-x86_64": {{ "url": "{}", "sha256": "{sha}", "size": 0 }} }} }} ] }}"#,
                archive.display()
            ),
        )
        .unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        resolve_template("9.9.9", "linux-x86_64", manifest.to_str().unwrap(), &data, &tx);
        let events: Vec<_> = rx.try_iter().collect();
        assert!(
            !events.iter().any(|e| matches!(e, TemplateProgress::Ready(_))),
            "a bundle with no player must not resolve — it would ship the editor"
        );
        let why = events
            .iter()
            .find_map(|e| match e {
                TemplateProgress::Failed(m) => Some(m.clone()),
                _ => None,
            })
            .expect("it fails rather than going quiet");
        assert!(
            why.contains("floptle-player"),
            "the failure has to name what is missing, got: {why}"
        );
    }

    /// A cached template is used as-is and never re-downloaded — the whole point
    /// of paying for the fetch once. With no cache and an unreachable manifest,
    /// the failure names the version rather than dying obscurely.
    #[test]
    fn a_cached_template_is_used_without_touching_the_network() {
        let data = temp("tpl-data");
        // The PLAYER is what "cached" means for an export: it is the binary a
        // build ships, so its presence is what lets the fetch be skipped.
        let bin = floptle_dist::template_player_binary(&data, "9.9.9", "windows-x86_64");
        floptle_vfs::create_dir_all(bin.parent().unwrap()).unwrap();
        floptle_vfs::write(&bin, "engine").unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        // A manifest URL that would fail loudly if it were ever consulted.
        resolve_template("9.9.9", "windows-x86_64", "/nonexistent/releases.json", &data, &tx);
        match rx.try_recv() {
            Ok(TemplateProgress::Ready(p)) => assert_eq!(p, bin),
            _ => panic!("a cached template must resolve immediately"),
        }

        let (tx, rx) = std::sync::mpsc::channel();
        resolve_template("0.0.1", "linux-x86_64", "/nonexistent/releases.json", &data, &tx);
        assert!(
            matches!(rx.try_recv(), Ok(TemplateProgress::Failed(e)) if e.contains("manifest")),
            "an unreadable manifest is a clear failure"
        );
        let _ = std::fs::remove_dir_all(&data);
    }

    /// An engine version with no published bundle is `Unpublished`, NOT `Failed` —
    /// that distinction is what lets a source checkout fall back to building one,
    /// which is the only way to export during engine development.
    #[test]
    fn an_unpublished_version_is_distinguishable_from_a_failure() {
        let data = temp("tpl-unpub");
        let manifest = data.join("releases.json");
        floptle_vfs::write(
            &manifest,
            r#"{ "versions": [ { "version": "1.0.0", "channel": "stable",
                 "artifacts": { "linux-x86_64": { "url": "u", "sha256": "a", "size": 1 } } } ] }"#,
        )
        .unwrap();
        let url = manifest.to_str().unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        resolve_template("0.11.0", "linux-x86_64", url, &data, &tx);
        assert!(
            matches!(rx.try_recv(), Ok(TemplateProgress::Unpublished(e)) if e.contains("0.11.0")),
            "an absent VERSION is unpublished"
        );

        let (tx, rx) = std::sync::mpsc::channel();
        resolve_template("1.0.0", "macos-aarch64", url, &data, &tx);
        assert!(
            matches!(rx.try_recv(), Ok(TemplateProgress::Unpublished(e)) if e.contains("macos")),
            "a published version missing THIS platform is also unpublished"
        );
        let _ = std::fs::remove_dir_all(&data);
    }

    /// The full template path with no network: a local-file artifact is
    /// downloaded (copied), checksum-verified, unpacked, and cached — and the
    /// second call serves the cache.
    #[test]
    fn a_template_is_fetched_verified_unpacked_and_then_cached() {
        use sha2::{Digest, Sha256};
        let data = temp("tpl-e2e");
        let archive = data.join("floptle-1.2.3-linux-x86_64.tar.gz");
        {
            let gz = flate2::write::GzEncoder::new(
                std::fs::File::create(&archive).unwrap(),
                flate2::Compression::default(),
            );
            let mut tar = tar::Builder::new(gz);
            // A real bundle carries BOTH: the editor the Hub runs, and the
            // player an export ships.
            for name in ["floptle", "floptle-player"] {
                let payload = b"#!/bin/sh\necho engine\n";
                let mut h = tar::Header::new_gnu();
                h.set_size(payload.len() as u64);
                h.set_mode(0o755);
                h.set_cksum();
                tar.append_data(&mut h, name, &payload[..]).unwrap();
            }
            tar.into_inner().unwrap().finish().unwrap();
        }
        let sha: String = {
            let mut h = Sha256::new();
            h.update(floptle_vfs::read(&archive).unwrap());
            h.finalize().iter().map(|b| format!("{b:02x}")).collect()
        };
        let manifest = data.join("releases.json");
        floptle_vfs::write(
            &manifest,
            format!(
                r#"{{ "versions": [ {{ "version": "1.2.3", "channel": "stable", "artifacts": {{
                     "linux-x86_64": {{ "url": "{}", "sha256": "{sha}", "size": 0 }} }} }} ] }}"#,
                archive.display()
            ),
        )
        .unwrap();
        let url = manifest.to_str().unwrap();

        let (tx, rx) = std::sync::mpsc::channel();
        resolve_template("1.2.3", "linux-x86_64", url, &data, &tx);
        let events: Vec<_> = rx.try_iter().collect();
        let bin = events
            .iter()
            .find_map(|e| match e {
                TemplateProgress::Ready(p) => Some(p.clone()),
                _ => None,
            })
            .expect("template resolves");
        assert_eq!(
            bin,
            floptle_dist::template_player_binary(&data, "1.2.3", "linux-x86_64"),
            "a template resolves to the PLAYER — that is the binary a build ships"
        );
        assert!(bin.is_file(), "the player binary is cached on disk");
        assert!(
            events.iter().any(|e| matches!(e, TemplateProgress::Verifying)),
            "the checksum is verified, not assumed"
        );

        // A corrupt manifest entry for the SAME archive must be rejected.
        floptle_vfs::write(
            &manifest,
            format!(
                r#"{{ "versions": [ {{ "version": "9.0.0", "channel": "stable", "artifacts": {{
                     "linux-x86_64": {{ "url": "{}", "sha256": "dead", "size": 0 }} }} }} ] }}"#,
                archive.display()
            ),
        )
        .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        resolve_template("9.0.0", "linux-x86_64", url, &data, &tx);
        assert!(
            rx.try_iter().any(|e| matches!(e, TemplateProgress::Failed(m) if m.contains("checksum"))),
            "a bad checksum must not produce a template"
        );
        assert!(
            !floptle_dist::template_binary(&data, "9.0.0", "linux-x86_64").exists(),
            "and nothing is left cached"
        );
        let _ = std::fs::remove_dir_all(&data);
    }
}
