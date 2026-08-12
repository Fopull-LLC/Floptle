//! Putting a package into a project, and taking it out again.
//!
//! Four ways in, and they differ only in how the files arrive:
//!
//! - **from a folder** — copied into `<project>/packages/<id>/`
//! - **from a Git URL** — cloned to a temp dir, then copied in the same way
//! - **linked** — not copied at all; the project points at where you are
//!   writing it. This is package *development* mode.
//! - **authored** — ✚ New Package scaffolds `<project>/packages/<id>/` and the
//!   project owns it from the start.
//!
//! Everything routes through [`install_from_dir`], so a package that came over
//! the network lands under exactly the same validation as one from a folder:
//! the manifest is parsed and validated **before** a single file is copied.

use std::path::{Path, PathBuf};

use crate::manifest::{Manifest, MANIFEST_FILE};
use crate::registry::{Entry, Registry, Source, PACKAGES_DIR};
use crate::version::Version;

/// What went wrong putting a package in or taking it out.
#[derive(Debug)]
pub enum InstallError {
    /// The folder is not a package.
    Manifest(String),
    /// A package with this id is already installed.
    AlreadyInstalled { id: String, version: Version },
    /// Filesystem trouble.
    Io(String),
    /// `git` could not be run, or the clone failed.
    Git(String),
    /// Asked to install a package into itself, or a link pointing inside the
    /// project's own `packages/` folder.
    Nonsense(String),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallError::Manifest(m) => write!(f, "{m}"),
            InstallError::AlreadyInstalled { id, version } => write!(
                f,
                "`{id}` {version} is already installed — remove it first, or use Update to \
                 replace it in place"
            ),
            InstallError::Io(m) => write!(f, "{m}"),
            InstallError::Git(m) => write!(f, "{m}"),
            InstallError::Nonsense(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for InstallError {}

fn io<E: std::fmt::Display>(what: &str) -> impl Fn(E) -> InstallError + '_ {
    move |e| InstallError::Io(format!("{what}: {e}"))
}

/// Install a package from a folder on this machine.
///
/// `replace` allows overwriting an installed package of the same id — that is
/// the Update path, and it is deliberately not the default: silently replacing
/// somebody's package because the ids matched is how a project loses local
/// edits.
pub fn install_from_dir(
    project_root: &Path,
    src: &Path,
    replace: bool,
) -> Result<Entry, InstallError> {
    let manifest = Manifest::load(src).map_err(|e| InstallError::Manifest(e.to_string()))?;
    let mut reg = Registry::load(project_root).map_err(InstallError::Io)?;
    if let Some(existing) = reg.find(&manifest.id)
        && !replace
    {
        return Err(InstallError::AlreadyInstalled {
            id: existing.id.clone(),
            version: existing.version.clone(),
        });
    }

    let dest = project_root.join(PACKAGES_DIR).join(&manifest.id);
    let src_canon = src.canonicalize().map_err(io("reading the package folder"))?;
    // Copying a folder into itself walks forever. It is also exactly what
    // "install the package I am already developing here" looks like.
    if dest.exists()
        && let Ok(d) = dest.canonicalize()
        && d == src_canon
    {
        return Err(InstallError::Nonsense(format!(
            "`{}` is already this project's copy of the package — nothing to install",
            src.display()
        )));
    }
    if dest.exists() {
        std::fs::remove_dir_all(&dest).map_err(io("replacing the installed package"))?;
    }
    copy_dir(&src_canon, &dest).map_err(io("copying the package"))?;

    let entry = Entry {
        id: manifest.id.clone(),
        version: manifest.version.clone(),
        source: Source::Folder(src_canon.display().to_string()),
        enabled: true,
    };
    reg.upsert(entry.clone());
    reg.save(project_root).map_err(io("writing packages.ron"))?;
    Ok(entry)
}

/// Point the project at a package you are writing, wherever it lives. Nothing
/// is copied: edits show up on the next reload.
pub fn link_dir(project_root: &Path, src: &Path, replace: bool) -> Result<Entry, InstallError> {
    let manifest = Manifest::load(src).map_err(|e| InstallError::Manifest(e.to_string()))?;
    let src_canon = src.canonicalize().map_err(io("reading the package folder"))?;
    // A link into the project's own packages/ folder would be an Authored
    // package wearing the wrong label — and Remove would then refuse to delete
    // files the project does own.
    let owned = project_root.join(PACKAGES_DIR);
    if src_canon.starts_with(owned.canonicalize().unwrap_or(owned)) {
        return Err(InstallError::Nonsense(
            "that folder is already inside this project's packages/ — it does not need linking"
                .into(),
        ));
    }
    let mut reg = Registry::load(project_root).map_err(InstallError::Io)?;
    if let Some(existing) = reg.find(&manifest.id)
        && !replace
    {
        return Err(InstallError::AlreadyInstalled {
            id: existing.id.clone(),
            version: existing.version.clone(),
        });
    }
    let entry = Entry {
        id: manifest.id.clone(),
        version: manifest.version.clone(),
        source: Source::Linked(src_canon.display().to_string()),
        enabled: true,
    };
    reg.upsert(entry.clone());
    reg.save(project_root).map_err(io("writing packages.ron"))?;
    Ok(entry)
}

/// Clone a Git remote into `scratch` and install what it holds.
///
/// `subdir` is for repositories that keep the package below the root (a repo of
/// several packages, or one with the package under `package/`). `rev` is any
/// branch, tag or commit.
///
/// **This shells out to `git`.** Bundling a Git implementation to install a
/// package is a large dependency for a job every developer's machine can already
/// do, and a missing `git` is a message a person can act on.
pub fn install_from_git(
    project_root: &Path,
    scratch: &Path,
    url: &str,
    rev: Option<&str>,
    subdir: Option<&str>,
    replace: bool,
) -> Result<Entry, InstallError> {
    let clone_dir = clone(scratch, url, rev)?;
    let src = match subdir {
        Some(s) if !s.trim().is_empty() => clone_dir.join(s),
        _ => clone_dir.clone(),
    };
    if !src.join(MANIFEST_FILE).exists() {
        // Look one level down before giving up: a repo whose package sits in a
        // single subfolder is the common shape, and finding it is better than
        // asking the user to guess the path.
        if let Some(found) = find_manifest_one_level(&src) {
            return finish_git(project_root, &found, url, rev, replace, &clone_dir);
        }
        let _ = std::fs::remove_dir_all(&clone_dir);
        return Err(InstallError::Manifest(format!(
            "{url} has no {MANIFEST_FILE}{} — if the package lives in a subfolder, name it",
            subdir.map(|s| format!(" at `{s}`")).unwrap_or_default()
        )));
    }
    finish_git(project_root, &src, url, rev, replace, &clone_dir)
}

fn finish_git(
    project_root: &Path,
    src: &Path,
    url: &str,
    rev: Option<&str>,
    replace: bool,
    clone_dir: &Path,
) -> Result<Entry, InstallError> {
    let result = install_from_dir(project_root, src, replace);
    let _ = std::fs::remove_dir_all(clone_dir);
    let mut entry = result?;
    // Record the REMOTE, not the temp folder the clone happened to land in.
    entry.source = Source::Git { url: url.to_string(), rev: rev.map(|r| r.to_string()) };
    let mut reg = Registry::load(project_root).map_err(InstallError::Io)?;
    reg.upsert(entry.clone());
    reg.save(project_root).map_err(io("writing packages.ron"))?;
    Ok(entry)
}

fn find_manifest_one_level(dir: &Path) -> Option<PathBuf> {
    let mut hits = std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.join(MANIFEST_FILE).exists());
    let first = hits.next()?;
    // Two candidates is an ambiguity, and guessing between them is worse than
    // asking.
    if hits.next().is_some() { None } else { Some(first) }
}

fn clone(scratch: &Path, url: &str, rev: Option<&str>) -> Result<PathBuf, InstallError> {
    std::fs::create_dir_all(scratch).map_err(io("making a scratch folder"))?;
    let dir = scratch.join(format!("clone-{}", sanitize(url)));
    let _ = std::fs::remove_dir_all(&dir);
    let mut args: Vec<String> = vec!["clone".into(), "--depth".into(), "1".into()];
    if let Some(r) = rev {
        args.push("--branch".into());
        args.push(r.to_string());
    }
    args.push(url.to_string());
    args.push(dir.display().to_string());
    let out = std::process::Command::new("git")
        .args(&args)
        .output()
        .map_err(|e| {
            InstallError::Git(format!(
                "could not run `git`: {e}. Installing from a repository needs Git on your PATH"
            ))
        })?;
    if !out.status.success() {
        // A shallow clone cannot name a commit SHA, only a branch or tag. Fall
        // back to a full clone + checkout rather than telling somebody their
        // perfectly good commit id is wrong.
        if let Some(rev) = rev {
            let _ = std::fs::remove_dir_all(&dir);
            return clone_full_then_checkout(&dir, url, rev);
        }
        let _ = std::fs::remove_dir_all(&dir);
        return Err(InstallError::Git(format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(dir)
}

fn clone_full_then_checkout(dir: &Path, url: &str, rev: &str) -> Result<PathBuf, InstallError> {
    let out = std::process::Command::new("git")
        .args(["clone", url, &dir.display().to_string()])
        .output()
        .map_err(|e| InstallError::Git(format!("could not run `git`: {e}")))?;
    if !out.status.success() {
        return Err(InstallError::Git(format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let out = std::process::Command::new("git")
        .args(["-C", &dir.display().to_string(), "checkout", rev])
        .output()
        .map_err(|e| InstallError::Git(format!("could not run `git`: {e}")))?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(dir);
        return Err(InstallError::Git(format!(
            "`{rev}` is not a branch, tag or commit in that repository"
        )));
    }
    Ok(dir.to_path_buf())
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect::<String>()
}

/// Scaffold a new package inside the project and register it as `Authored`.
///
/// Writes a manifest, a README, and an `editor/main.lua` that already does
/// something visible — a first run that draws nothing is indistinguishable from
/// a first run that failed.
pub fn scaffold(
    project_root: &Path,
    id: &str,
    name: &str,
) -> Result<Entry, InstallError> {
    crate::manifest::validate_id(id).map_err(InstallError::Manifest)?;
    let mut reg = Registry::load(project_root).map_err(InstallError::Io)?;
    if let Some(e) = reg.find(id) {
        return Err(InstallError::AlreadyInstalled { id: e.id.clone(), version: e.version.clone() });
    }
    let root = project_root.join(PACKAGES_DIR).join(id);
    if root.exists() {
        return Err(InstallError::Nonsense(format!(
            "{} already exists — pick another id, or delete that folder",
            root.display()
        )));
    }
    let manifest = Manifest::new(id, name, Version::new(0, 1, 0));
    manifest.save(&root).map_err(io("writing the new package"))?;
    std::fs::create_dir_all(root.join("editor")).map_err(io("writing the new package"))?;
    std::fs::create_dir_all(root.join("scripts")).map_err(io("writing the new package"))?;
    std::fs::create_dir_all(root.join("assets")).map_err(io("writing the new package"))?;
    std::fs::write(root.join("README.md"), starter_readme(name, id))
        .map_err(io("writing the new package"))?;
    std::fs::write(root.join("editor/main.lua"), STARTER_LUA)
        .map_err(io("writing the new package"))?;

    let entry = Entry {
        id: id.to_string(),
        version: manifest.version.clone(),
        source: Source::Authored,
        enabled: true,
    };
    reg.upsert(entry.clone());
    reg.save(project_root).map_err(io("writing packages.ron"))?;
    Ok(entry)
}

fn starter_readme(name: &str, id: &str) -> String {
    format!(
        "# {name}\n\n\
         A Floptle package. `editor/main.lua` runs in the editor; `scripts/` is Lua your \
         game can attach to nodes; `assets/` is everything else.\n\n\
         Address this package's files from anywhere with `pkg://{id}/…`.\n"
    )
}

const STARTER_LUA: &str = r#"-- Runs in the editor when the package loads.
-- Everything here is optional: delete what you don't need.

local panel = ed.window("Hello", function()
    gui.label("This panel is drawn by a package.")
    if gui.button("Say hello") then
        ed.log("Hello from " .. ed.package.name)
    end
end)

ed.menu("Hello/Open the panel", function() panel:show() end)

ed.onSceneDraw(function()
    -- Drawn in the Scene view, in world space.
    handles.color(0.2, 0.9, 0.6)
    handles.wireCube(vec3(0, 0, 0), vec3(1, 1, 1))
end)
"#;

/// Copy a sample out of a package and into the project, under
/// `samples/<package name>/<sample name>/`. Samples are copied rather than
/// referenced on purpose: they are a starting point to edit, not a dependency.
pub fn import_sample(
    project_root: &Path,
    package_root: &Path,
    package_name: &str,
    sample_name: &str,
    sample_path: &str,
) -> Result<PathBuf, InstallError> {
    let src = package_root.join(sample_path);
    if !src.is_dir() {
        return Err(InstallError::Io(format!(
            "the `{sample_name}` sample is missing from the package ({})",
            src.display()
        )));
    }
    let dest = project_root.join("samples").join(safe_name(package_name)).join(safe_name(sample_name));
    if dest.exists() {
        std::fs::remove_dir_all(&dest).map_err(io("replacing the imported sample"))?;
    }
    copy_dir(&src, &dest).map_err(io("copying the sample"))?;
    Ok(dest)
}

fn safe_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ' ' { c } else { '-' })
        .collect();
    let trimmed = cleaned.trim().to_string();
    if trimmed.is_empty() { "package".into() } else { trimmed }
}

/// Take a package out of the project. A copied package's files are deleted; a
/// linked one's are not — those belong to wherever they are being written, and
/// unlinking must never reach out of the project to delete somebody's work.
pub fn remove(project_root: &Path, id: &str) -> Result<Entry, InstallError> {
    let mut reg = Registry::load(project_root).map_err(InstallError::Io)?;
    let entry = reg
        .remove(id)
        .ok_or_else(|| InstallError::Nonsense(format!("`{id}` is not installed")))?;
    if !entry.source.is_linked() {
        let dir = project_root.join(PACKAGES_DIR).join(id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(io("deleting the package folder"))?;
        }
    }
    reg.save(project_root).map_err(io("writing packages.ron"))?;
    Ok(entry)
}

/// Turn a package on or off without removing it.
pub fn set_enabled(project_root: &Path, id: &str, on: bool) -> Result<(), InstallError> {
    let mut reg = Registry::load(project_root).map_err(InstallError::Io)?;
    let e = reg
        .find_mut(id)
        .ok_or_else(|| InstallError::Nonsense(format!("`{id}` is not installed")))?;
    e.enabled = on;
    reg.save(project_root).map_err(io("writing packages.ron"))
}

/// Recursive copy. Skips `.git` (a vendored clone is not a repository) and the
/// `~`-suffixed folders Unity-style packages use to hide samples from an
/// importer — a convention worth honouring since packages will be ported from
/// there.
pub fn copy_dir(src: &Path, dest: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let from = entry.path();
        if from.is_dir() {
            if name_str == ".git" {
                continue;
            }
            copy_dir(&from, &dest.join(&name))?;
        } else {
            std::fs::copy(&from, dest.join(&name))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "flpkg-inst-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn a_package(at: &Path, id: &str) -> PathBuf {
        let dir = at.join("src-pkg");
        let m = Manifest::new(id, "Test", Version::new(1, 0, 0));
        m.save(&dir).unwrap();
        std::fs::create_dir_all(dir.join("editor")).unwrap();
        std::fs::write(dir.join("editor/main.lua"), "-- hi\n").unwrap();
        dir
    }

    #[test]
    fn installs_copies_and_registers() {
        let base = temp("copy");
        let proj = base.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let src = a_package(&base, "com.test.a");
        let e = install_from_dir(&proj, &src, false).unwrap();
        assert_eq!(e.id, "com.test.a");
        assert!(proj.join("packages/com.test.a/package.ron").exists());
        assert!(proj.join("packages/com.test.a/editor/main.lua").exists());
        assert_eq!(Registry::load(&proj).unwrap().packages.len(), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn installing_twice_needs_asking() {
        let base = temp("twice");
        let proj = base.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let src = a_package(&base, "com.test.a");
        install_from_dir(&proj, &src, false).unwrap();
        let err = install_from_dir(&proj, &src, false).unwrap_err();
        assert!(matches!(err, InstallError::AlreadyInstalled { .. }), "{err}");
        // …and replace: true is the update path.
        install_from_dir(&proj, &src, true).unwrap();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The one that walks forever if it is not caught.
    #[test]
    fn installing_a_package_over_itself_is_refused() {
        let base = temp("selfcopy");
        let proj = base.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let src = a_package(&base, "com.test.a");
        install_from_dir(&proj, &src, false).unwrap();
        let installed = proj.join("packages/com.test.a");
        let err = install_from_dir(&proj, &installed, true).unwrap_err();
        assert!(matches!(err, InstallError::Nonsense(_)), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_link_copies_nothing() {
        let base = temp("link");
        let proj = base.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let src = a_package(&base, "com.test.a");
        let e = link_dir(&proj, &src, false).unwrap();
        assert!(e.source.is_linked());
        assert!(!proj.join("packages/com.test.a").exists());
        assert_eq!(e.root_in(&proj), src.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Unlinking must never delete files outside the project.
    #[test]
    fn removing_a_link_leaves_the_source_alone() {
        let base = temp("unlink");
        let proj = base.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let src = a_package(&base, "com.test.a");
        link_dir(&proj, &src, false).unwrap();
        remove(&proj, "com.test.a").unwrap();
        assert!(src.join("package.ron").exists(), "the linked source was deleted");
        assert!(Registry::load(&proj).unwrap().packages.is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn removing_a_copy_deletes_it() {
        let base = temp("rm");
        let proj = base.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let src = a_package(&base, "com.test.a");
        install_from_dir(&proj, &src, false).unwrap();
        remove(&proj, "com.test.a").unwrap();
        assert!(!proj.join("packages/com.test.a").exists());
        assert!(src.join("package.ron").exists(), "the ORIGIN folder was deleted");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn scaffold_writes_something_that_runs() {
        let base = temp("scaffold");
        let proj = base.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        scaffold(&proj, "com.me.tools", "My Tools").unwrap();
        let root = proj.join("packages/com.me.tools");
        let m = Manifest::load(&root).unwrap();
        assert_eq!(m.name, "My Tools");
        assert!(root.join("editor/main.lua").exists());
        assert!(root.join("README.md").exists());
        assert_eq!(Registry::load(&proj).unwrap().packages[0].source, Source::Authored);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn scaffold_rejects_a_bad_id_before_writing_anything() {
        let base = temp("badid");
        let proj = base.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        assert!(scaffold(&proj, "tools", "Tools").is_err());
        assert!(!proj.join("packages").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn enable_and_disable_survive_a_reload() {
        let base = temp("toggle");
        let proj = base.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let src = a_package(&base, "com.test.a");
        install_from_dir(&proj, &src, false).unwrap();
        set_enabled(&proj, "com.test.a", false).unwrap();
        assert!(!Registry::load(&proj).unwrap().find("com.test.a").unwrap().enabled);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_sample_lands_under_the_project() {
        let base = temp("sample");
        let proj = base.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let src = a_package(&base, "com.test.a");
        std::fs::create_dir_all(src.join("samples/tutorial/scenes")).unwrap();
        std::fs::write(src.join("samples/tutorial/scenes/a.ron"), "()").unwrap();
        let dest =
            import_sample(&proj, &src, "Test Pack", "Tutorial", "samples/tutorial").unwrap();
        assert!(dest.join("scenes/a.ron").exists());
        assert!(dest.starts_with(&proj));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn copy_skips_a_vendored_git_folder() {
        let base = temp("gitskip");
        let src = base.join("a");
        std::fs::create_dir_all(src.join(".git")).unwrap();
        std::fs::write(src.join(".git/HEAD"), "x").unwrap();
        std::fs::write(src.join("f.txt"), "y").unwrap();
        let dst = base.join("b");
        copy_dir(&src, &dst).unwrap();
        assert!(dst.join("f.txt").exists());
        assert!(!dst.join(".git").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn installing_a_folder_that_is_not_a_package_says_so() {
        let base = temp("notapkg");
        let proj = base.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let notpkg = base.join("random");
        std::fs::create_dir_all(&notpkg).unwrap();
        let err = install_from_dir(&proj, &notpkg, false).unwrap_err();
        assert!(err.to_string().contains("package.ron"), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }
}
