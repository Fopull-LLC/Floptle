//! File ⏵ Export Game… — stamping a runnable build for any platform.
//!
//! # Why there is no compiler here
//!
//! An exported build is the engine binary + the project's assets + a
//! `floptle-game.ron` manifest that flips the binary into player mode. **Nothing
//! about a project is compiled in.** So the binary a build needs is not
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

use crate::Editor;
use floptle_script::LogLevel;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Instant;

/// The manifest File ⏵ Export Game… writes next to the binary. Its presence
/// turns the binary into a game player; `project` is the assets folder
/// relative to the manifest.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct GameManifest {
    pub(crate) title: String,
    pub(crate) project: String,
}

/// A `floptle-game.ron` beside the running binary, if any → (manifest, its dir).
pub(crate) fn load_game_manifest() -> Option<(GameManifest, PathBuf)> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let text = std::fs::read_to_string(dir.join("floptle-game.ron")).ok()?;
    match ron::from_str::<GameManifest>(&text) {
        Ok(m) => Some((m, dir)),
        Err(e) => {
            eprintln!("floptle-game.ron next to the binary is invalid ({e}); starting as editor");
            None
        }
    }
}

/// How an Export Game… target obtains its engine binary.
pub(crate) enum ExportKind {
    /// Copy the running binary — always available, always this platform.
    SelfBinary,
    /// A published release bundle for `platform`, downloaded and cached.
    /// `cross` is the Rust triple used ONLY as a source-checkout fallback when
    /// this engine version has no published bundle (macOS has none: it cannot
    /// be cross-compiled, which is exactly why templates exist).
    Template { platform: &'static str, cross: Option<&'static str> },
}

pub(crate) struct ExportTarget {
    pub(crate) label: &'static str,
    pub(crate) kind: ExportKind,
    pub(crate) exe_suffix: &'static str,
    pub(crate) readme: Option<&'static str>,
}

/// Shipped beside a macOS build: an unsigned binary off the App Store is
/// quarantined, and the failure ("damaged and can't be opened") reads like a
/// broken download rather than a policy.
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

/// Every target Export Game… offers. "This machine" first (no download); the
/// rest are published bundles. All four platforms are symmetric — the host you
/// export FROM stopped mattering when the compiler left.
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
];

/// Progress from the worker that resolves an export template.
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
pub(crate) struct ExportJob {
    pub(crate) out_dir: String,
    pub(crate) title: String,
    pub(crate) target: usize,
    pub(crate) started: Instant,
    pub(crate) work: JobWork,
}

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
pub(crate) fn resolve_template(
    version: &str,
    platform: &str,
    manifest_url: &str,
    data_dir: &Path,
    tx: &Sender<TemplateProgress>,
) {
    let msg = match run_resolve(version, platform, manifest_url, data_dir, tx) {
        Ok(bin) => TemplateProgress::Ready(bin),
        Err(e) => e,
    };
    let _ = tx.send(msg);
}

fn run_resolve(
    version: &str,
    platform: &str,
    manifest_url: &str,
    data_dir: &Path,
    tx: &Sender<TemplateProgress>,
) -> Result<PathBuf, TemplateProgress> {
    let cached = floptle_dist::template_binary(data_dir, version, platform);
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
    std::fs::create_dir_all(&staging)
        .map_err(|e| TemplateProgress::Failed(format!("template dir: {e}")))?;
    let bin_name = floptle_dist::editor_bin_name_for(platform);
    let staged = (|| {
        floptle_dist::unpack(&archive, &staging)?;
        if !staging.join(&bin_name).is_file() {
            return Err(format!("the {platform} bundle contains no {bin_name}"));
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
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::rename(&staging, &dest)
        .map_err(|e| TemplateProgress::Failed(format!("commit template: {e}")))?;
    let _ = std::fs::remove_file(&archive);
    let bin = dest.join(&bin_name);
    floptle_dist::set_executable(&bin);
    Ok(bin)
}

/// The engine source checkout this editor was built from (compiled-in path — a
/// dev machine). `None` if it's gone, which is the normal case for a Hub install.
pub(crate) fn repo_root() -> Option<PathBuf> {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent()?.parent()?;
    repo.join("Cargo.toml").is_file().then(|| repo.to_path_buf())
}

/// Where a cross-built fallback binary lands.
fn cross_binary_path(triple: Option<&str>, exe_suffix: &str) -> Option<PathBuf> {
    let target_dir =
        std::env::current_exe().ok().and_then(|e| Some(e.parent()?.parent()?.to_path_buf()))?;
    let name = format!("floptle{exe_suffix}");
    Some(match triple {
        Some(t) => target_dir.join(t).join("release").join(name),
        None => target_dir.join("release").join(name),
    })
}

/// Spawn the background release `cargo build` used as the source-checkout
/// fallback. Needs the engine source checkout and the rustup + C toolchains.
fn spawn_export_build(triple: Option<&str>, log: &Path) -> Result<std::process::Child, String> {
    let repo = repo_root()
        .ok_or_else(|| "a fallback build needs the engine source checkout".to_string())?;
    let logfile = std::fs::File::create(log).map_err(|e| format!("build log: {e}"))?;
    let mut cmd = std::process::Command::new("cargo");
    cmd.current_dir(repo)
        .args(["build", "--release", "-p", "floptle-editor"])
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
                std::fs::create_dir_all(&shim).map_err(|e| format!("shim dir: {e}"))?;
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
fn ships(name: &str, at_root: bool) -> bool {
    !(name.starts_with('.') || (at_root && RUNTIME_DIRS.contains(&name)))
}

/// Recursive copy for the export. Returns the number of files copied.
fn copy_tree(src: &Path, dst: &Path, at_root: bool) -> std::io::Result<u64> {
    std::fs::create_dir_all(dst)?;
    let mut n = 0;
    for entry in std::fs::read_dir(src)?.flatten() {
        let name = entry.file_name();
        if !ships(&name.to_string_lossy(), at_root) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if entry.file_type()?.is_dir() {
            n += copy_tree(&from, &to, false)?;
        } else {
            std::fs::copy(&from, &to)?;
            n += 1;
        }
    }
    Ok(n)
}

/// Text files that can carry an asset reference.
fn is_texty(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()),
        Some("ron" | "lua" | "flsl" | "json" | "toml")
    )
}

/// What a portability scan found.
#[derive(Default, Debug, PartialEq)]
pub(crate) struct Portability {
    /// Files rewritten from an absolute path into the project to a relative one.
    pub(crate) rewritten: usize,
    /// Absolute paths that point OUTSIDE the project — unfixable here, because
    /// the file they name isn't in the build at all.
    pub(crate) foreign: Vec<String>,
}

/// Make a copied project portable.
///
/// An absolute asset path resolves as-is with no rescue (see
/// `project::resolve_asset_path`), so a build carrying one is broken on every
/// machine except the one that exported it — silently, since a missing model
/// just doesn't appear. Rewriting the project root's own prefix is safe by
/// construction: that string can only ever be a path INTO the project.
///
/// Paths outside the project can't be repaired — the file isn't in the build —
/// so they're reported instead.
pub(crate) fn make_portable(shipped: &Path, project_root: &Path) -> Portability {
    let mut out = Portability::default();
    let Some(root) = project_root.to_str() else { return out };
    let prefix = format!("{}{}", root.trim_end_matches(std::path::MAIN_SEPARATOR), std::path::MAIN_SEPARATOR);
    let mut stack = vec![shipped.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if !is_texty(&p) {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else { continue };
            if text.contains(&prefix) {
                let fixed = text.replace(&prefix, "");
                if std::fs::write(&p, fixed).is_ok() {
                    out.rewritten += 1;
                }
            }
            for abs in absolute_refs(&std::fs::read_to_string(&p).unwrap_or_default()) {
                if !out.foreign.contains(&abs) {
                    out.foreign.push(abs);
                }
            }
        }
    }
    out.foreign.sort();
    out
}

/// Quoted absolute-looking paths in a text file (`"/x/y"`, `"C:\x"`).
fn absolute_refs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for chunk in text.split('"').skip(1).step_by(2) {
        let is_abs = chunk.starts_with('/')
            || (chunk.len() > 2
                && chunk.as_bytes()[1] == b':'
                && matches!(chunk.as_bytes()[2], b'\\' | b'/'));
        // A bare "/" or a URL is not an asset reference.
        if is_abs && chunk.len() > 1 && !chunk.contains("://") && !out.contains(&chunk.to_string()) {
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
    if direct.is_file() {
        return Some(direct);
    }
    let scenes = project_root.join("scenes");
    [scenes.join(format!("{entry}.ron")), scenes.join(entry)].into_iter().find(|c| c.is_file())
}

/// Stamp out a runnable build: an engine binary + the project's assets + the
/// `floptle-game.ron` manifest that flips it into player mode.
pub(crate) fn export_game_with(
    project_root: &Path,
    out: &Path,
    title: &str,
    binary: &Path,
    target: &ExportTarget,
) -> Result<(String, PathBuf), String> {
    let exe_suffix = target.exe_suffix;
    std::fs::create_dir_all(out).map_err(|e| format!("create {}: {e}", out.display()))?;
    let proj = project_root.canonicalize().map_err(|e| format!("project dir: {e}"))?;
    let out_c = out.canonicalize().map_err(|e| format!("export dir: {e}"))?;
    if out_c.starts_with(&proj) {
        return Err("the export folder can't be inside the project (it would copy itself)".into());
    }
    // A build that can't find its chosen entry scene is dead on arrival — catch
    // it at export time, not on a player's machine.
    let cfg = floptle_scene::load_project(&proj.join("project.ron"));
    if let Some(entry) = cfg.entry_scene.as_deref()
        && !entry.trim().is_empty()
        && resolve_entry_scene(&proj, entry).is_none()
    {
        return Err(format!(
            "the project's entry scene ({entry}) doesn't exist — pick one in \
             Edit ⏵ Project Settings"
        ));
    }
    // Binary name from the title: filesystem-safe, the TARGET's suffix.
    let stem: String = title
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let stem = stem.trim_matches('_');
    let stem = if stem.is_empty() { "game" } else { stem };
    let mut exe_name = format!("{stem}{exe_suffix}");
    // The shipped project folder is literally named `assets` — an exe resolving
    // to that same name (a project rooted at `assets/`, exported for a
    // suffix-less target) would collide with it and corrupt the build.
    if exe_name == "assets" {
        exe_name = "game".into();
    }
    // The build's `assets/` copy is wholly owned by the export: clear the
    // previous one so files deleted from the project don't linger in shipped
    // builds (and a stale FILE named `assets` — the old broken-export
    // artifact — doesn't block the copy).
    let ship_assets = out_c.join("assets");
    if ship_assets.is_dir() {
        std::fs::remove_dir_all(&ship_assets).map_err(|e| format!("clear old assets copy: {e}"))?;
    } else if ship_assets.exists() {
        std::fs::remove_file(&ship_assets).map_err(|e| format!("clear old assets copy: {e}"))?;
    }
    // Everything the game needs ships BEFORE the binary, and the binary ships
    // LAST — a failed export must never leave a runnable-looking exe that,
    // missing its floptle-game.ron, silently boots as the EDITOR.
    let files = copy_tree(&proj, &ship_assets, true).map_err(|e| format!("copy assets: {e}"))?;
    let port = make_portable(&ship_assets, &proj);
    if let Some(tpl) = target.readme {
        std::fs::write(out_c.join("README.txt"), tpl.replace("{exe}", &exe_name))
            .map_err(|e| format!("write README: {e}"))?;
    }
    let manifest = GameManifest { title: title.to_string(), project: "assets".into() };
    let text = ron::ser::to_string_pretty(&manifest, ron::ser::PrettyConfig::default())
        .map_err(|e| format!("manifest: {e}"))?;
    std::fs::write(out_c.join("floptle-game.ron"), text)
        .map_err(|e| format!("write manifest: {e}"))?;
    let shipped = out_c.join(&exe_name);
    std::fs::copy(binary, &shipped).map_err(|e| format!("copy binary: {e}"))?;
    // A CI artifact may have lost its executable bit in transit — restore it
    // (only meaningful for unix-family targets; .exe doesn't care).
    floptle_dist::set_executable(&shipped);

    let mut msg = format!("exported {exe_name} + {files} asset file(s) to {}", out_c.display());
    if port.rewritten > 0 {
        msg.push_str(&format!(
            " — made {} file(s) portable (absolute paths into the project)",
            port.rewritten
        ));
    }
    if !port.foreign.is_empty() {
        msg.push_str(&format!(
            " — ⚠ {} reference(s) point OUTSIDE the project and will be missing on other \
             machines: {}",
            port.foreign.len(),
            port.foreign.join(", ")
        ));
    }
    Ok((msg, out_c))
}

/// Headless `--export <PROJECT> <OUT> <PLATFORM>`: stamp a build without a
/// window or a GPU. Same code the dialog drives — the template resolution just
/// blocks instead of being polled — so CI and scripts get exactly the editor's
/// behaviour, and this path is what makes the feature verifiable end to end.
///
/// `PLATFORM` is a release artifact key (`windows-x86_64`, `macos-aarch64`, …)
/// or `host` for this machine.
pub(crate) fn headless_export(project: &Path, out: &Path, platform: &str, title: &str) -> i32 {
    let version = crate::distribution_version();
    let target = if platform == "host" {
        &EXPORT_TARGETS[0]
    } else {
        match EXPORT_TARGETS
            .iter()
            .find(|t| matches!(t.kind, ExportKind::Template { platform: p, .. } if p == platform))
        {
            Some(t) => t,
            None => {
                eprintln!(
                    "unknown platform {platform:?} — expected `host` or one of: {}",
                    floptle_dist::PLATFORMS.join(", ")
                );
                return 2;
            }
        }
    };
    let binary = match target.kind {
        ExportKind::SelfBinary => match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("export failed: {e}");
                return 1;
            }
        },
        ExportKind::Template { platform, .. } => {
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
    match export_game_with(project, out, title, &binary, target) {
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
                    let result =
                        std::env::current_exe().map_err(|e| e.to_string()).and_then(|exe| {
                            export_game_with(&self.project_root, Path::new(&dir), &title, &exe, t)
                        });
                    self.finish_export(result);
                }
            }
            ExportKind::Template { platform, .. } => {
                self.begin_template_export(platform, dir, title, target)
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
        let cached = floptle_dist::template_binary(&data, &version, platform).is_file();
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
            let r =
                export_game_with(&self.project_root, Path::new(&job.out_dir), &job.title, &bin, t)
                    .map(|(m, d)| {
                        (format!("{m} (in {:.0} s)", job.started.elapsed().as_secs_f32()), d)
                    });
            return Some(Some(r));
        }
        let why = ended.unwrap_or_else(|| "template resolution ended without a result".into());
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
            ExportKind::SelfBinary => None,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("floptle-export-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
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

    /// Export = binary + assets (dot-entries skipped) + a manifest that parses
    /// back and points at the copied project.
    #[test]
    fn export_stamps_a_runnable_build() {
        let proj = temp("proj");
        std::fs::create_dir_all(proj.join("scenes")).unwrap();
        std::fs::write(proj.join("project.ron"), "()").unwrap();
        std::fs::write(proj.join("scenes/first.ron"), "()").unwrap();
        std::fs::create_dir_all(proj.join(".floptle")).unwrap();
        std::fs::write(proj.join(".floptle/cache.bin"), "x").unwrap();
        std::fs::write(proj.join(".luarc.json"), "{}").unwrap();
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
            ron::from_str(&std::fs::read_to_string(out.join("floptle-game.ron")).unwrap())
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
        let readme = std::fs::read_to_string(out2.join("README.txt")).unwrap();
        assert!(readme.contains("./Sea_Game"), "README names the actual binary: {readme}");
        assert!(readme.contains("com.apple.quarantine"));

        for d in [&proj, &out, &out2] {
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
        std::fs::write(proj.join("project.ron"), "()").unwrap();
        let out = temp("out-collide");
        std::fs::write(out.join("assets"), "old broken binary").unwrap();

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
        std::fs::write(proj.join("project.ron"), "()").unwrap();
        std::fs::create_dir_all(proj.join("save")).unwrap();
        std::fs::write(proj.join("save/main.ron"), "{}").unwrap();
        std::fs::create_dir_all(proj.join("replays")).unwrap();
        std::fs::write(proj.join("replays/r1.log"), "x").unwrap();
        std::fs::create_dir_all(proj.join("scenes/save")).unwrap();
        std::fs::write(proj.join("scenes/save/level.ron"), "()").unwrap();
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
        std::fs::create_dir_all(proj.join("models")).unwrap();
        std::fs::write(proj.join("models/hero.glb"), "glb").unwrap();
        std::fs::write(proj.join("project.ron"), "()").unwrap();
        let root = proj.canonicalize().unwrap();
        std::fs::write(
            proj.join("hero.anim.ron"),
            format!("(source: \"{}/models/hero.glb\", clip: \"idle\")", root.display()),
        )
        .unwrap();
        std::fs::write(
            proj.join("stage.ron"),
            "(mesh: \"/elsewhere/on/disk/tree.glb\", url: \"https://example.com/x\")",
        )
        .unwrap();
        let out = temp("out-abs");

        let me = std::env::current_exe().unwrap();
        let (msg, _) = export_game_with(&proj, &out, "G", &me, &EXPORT_TARGETS[0]).expect("export");

        let anim = std::fs::read_to_string(out.join("assets/hero.anim.ron")).unwrap();
        assert!(anim.contains("\"models/hero.glb\""), "rewritten to project-relative: {anim}");
        assert!(!anim.contains(&root.display().to_string()), "no absolute path survives: {anim}");
        assert!(msg.contains("portable"), "the report mentions the rewrite: {msg}");

        assert!(msg.contains("OUTSIDE the project"), "foreign refs are reported: {msg}");
        assert!(msg.contains("/elsewhere/on/disk/tree.glb"), "and named: {msg}");
        assert!(!msg.contains("https://example.com"), "a URL is not an asset path: {msg}");

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
        std::fs::create_dir_all(proj.join("scenes")).unwrap();
        std::fs::write(proj.join("scenes/menu.ron"), "()").unwrap();
        std::fs::write(proj.join("scenes/first.ron"), "()").unwrap();

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

    /// A cached template is used as-is and never re-downloaded — the whole point
    /// of paying for the fetch once. With no cache and an unreachable manifest,
    /// the failure names the version rather than dying obscurely.
    #[test]
    fn a_cached_template_is_used_without_touching_the_network() {
        let data = temp("tpl-data");
        let bin = floptle_dist::template_binary(&data, "9.9.9", "windows-x86_64");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, "engine").unwrap();

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
        std::fs::write(
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
            let payload = b"#!/bin/sh\necho engine\n";
            let mut h = tar::Header::new_gnu();
            h.set_size(payload.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            tar.append_data(&mut h, "floptle", &payload[..]).unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }
        let sha: String = {
            let mut h = Sha256::new();
            h.update(std::fs::read(&archive).unwrap());
            h.finalize().iter().map(|b| format!("{b:02x}")).collect()
        };
        let manifest = data.join("releases.json");
        std::fs::write(
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
        assert_eq!(bin, floptle_dist::template_binary(&data, "1.2.3", "linux-x86_64"));
        assert!(bin.is_file(), "the engine binary is cached on disk");
        assert!(
            events.iter().any(|e| matches!(e, TemplateProgress::Verifying)),
            "the checksum is verified, not assumed"
        );

        // A corrupt manifest entry for the SAME archive must be rejected.
        std::fs::write(
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
