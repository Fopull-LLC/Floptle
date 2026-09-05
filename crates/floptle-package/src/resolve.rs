//! Turning a project's installed list into a load order — or into a list of
//! reasons it cannot have one.
//!
//! **Nothing here stops the editor.** A package with a broken manifest, a
//! missing dependency or an engine requirement this build does not meet is
//! *skipped*, loudly, and the rest of the project loads. A project should not
//! become unopenable because one package went wrong; the whole point of
//! `enabled: false` and of this list is that the person can see what happened
//! and act.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::manifest::Manifest;
use crate::registry::{Entry, Registry};
use crate::version::Version;

/// A package that is installed, enabled, valid, and where its files are.
#[derive(Clone, Debug)]
pub struct Loaded {
    pub entry: Entry,
    pub manifest: Manifest,
    /// Absolute path to the package's folder.
    pub root: PathBuf,
}

impl Loaded {
    pub fn id(&self) -> &str {
        &self.manifest.id
    }

    /// Does this package declare `p`? What the editor asks before handing an
    /// extension the matching API.
    pub fn grants(&self, p: crate::manifest::Permission) -> bool {
        self.manifest.grants(p)
    }

    /// Every `.lua` under the manifest's editor folders, sorted, so a package
    /// with several files loads in a stable order rather than in whatever order
    /// the filesystem answers in.
    pub fn editor_scripts(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for dir in self.manifest.dirs_that_exist(&self.root, crate::manifest::DirKind::Editor) {
            collect_lua(&dir, &mut out);
        }
        out.sort();
        out
    }
}

fn collect_lua(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = floptle_vfs::read_dir(dir) else { return };
    for e in rd {
        let p = e.path();
        if e.is_dir() {
            collect_lua(&p, out);
        } else if p.extension().is_some_and(|x| x == "lua") {
            out.push(p);
        }
    }
}

/// How bad a problem is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Severity {
    /// The package did not load.
    Error,
    /// It loaded, but something is worth knowing.
    Warning,
}

/// Something the person who installed these packages should be told.
#[derive(Clone, Debug)]
pub struct Problem {
    /// Which package, when it is about one.
    pub id: Option<String>,
    pub severity: Severity,
    pub message: String,
}

/// What [`resolve`] found.
#[derive(Clone, Debug, Default)]
pub struct LoadReport {
    /// Enabled, valid packages in **dependency order**: a package always
    /// appears after everything it depends on, so an extension's `require`
    /// finds what it needs already loaded.
    pub loaded: Vec<Loaded>,
    /// Installed but not loaded, and why. Includes the ones simply switched
    /// off, so the package list has one source of truth for every row's state.
    pub problems: Vec<Problem>,
    /// Installed and deliberately off.
    pub disabled: Vec<Entry>,
}

impl LoadReport {
    pub fn errors(&self) -> impl Iterator<Item = &Problem> {
        self.problems.iter().filter(|p| p.severity == Severity::Error)
    }

    pub fn find(&self, id: &str) -> Option<&Loaded> {
        self.loaded.iter().find(|l| l.id() == id)
    }
}

/// Read the project's installed list, load every enabled package's manifest,
/// check versions and dependencies, and order what survives.
///
/// `engine_version` is this build's version, checked against each manifest's
/// `engine` range.
pub fn resolve(project_root: &Path, engine_version: &Version) -> LoadReport {
    let mut report = LoadReport::default();
    let reg = match Registry::load(project_root) {
        Ok(r) => r,
        Err(e) => {
            report.problems.push(Problem { id: None, severity: Severity::Error, message: e });
            return report;
        }
    };

    // ---- 1. read every enabled package's manifest -------------------------
    let mut candidates: Vec<Loaded> = Vec::new();
    for entry in reg.packages {
        if !entry.enabled {
            report.disabled.push(entry);
            continue;
        }
        let root = entry.root_in(project_root);
        let manifest = match Manifest::load(&root) {
            Ok(m) => m,
            Err(e) => {
                report.problems.push(Problem {
                    id: Some(entry.id.clone()),
                    severity: Severity::Error,
                    message: format!("{}: {}", entry.id, e.message),
                });
                continue;
            }
        };
        if manifest.id != entry.id {
            report.problems.push(Problem {
                id: Some(entry.id.clone()),
                severity: Severity::Error,
                message: format!(
                    "`{}` is installed as `{}` but its manifest says `{}` — the folder and the \
                     list disagree about which package this is",
                    entry.id, entry.id, manifest.id
                ),
            });
            continue;
        }
        if manifest.version != entry.version {
            // Not fatal: editing a linked package's version while it is
            // installed is exactly what a package author does all day. The
            // MANIFEST wins, and the list gets corrected.
            report.problems.push(Problem {
                id: Some(entry.id.clone()),
                severity: Severity::Warning,
                message: format!(
                    "{} is listed as {} but is really {} — the package's own version wins",
                    entry.id, entry.version, manifest.version
                ),
            });
        }
        if let Some(req) = &manifest.engine
            && !req.matches_engine(engine_version)
        {
            report.problems.push(Problem {
                id: Some(entry.id.clone()),
                severity: Severity::Error,
                message: format!(
                    "{} needs Floptle {} and this is {engine_version}",
                    manifest.name,
                    req.as_str()
                ),
            });
            continue;
        }
        candidates.push(Loaded { entry, manifest, root });
    }

    // ---- 2. dependencies --------------------------------------------------
    let have: HashMap<String, Version> =
        candidates.iter().map(|c| (c.manifest.id.clone(), c.manifest.version.clone())).collect();
    let mut rejected: HashSet<String> = HashSet::new();
    for c in &candidates {
        for dep in &c.manifest.dependencies {
            match have.get(&dep.id) {
                None => {
                    rejected.insert(c.manifest.id.clone());
                    report.problems.push(Problem {
                        id: Some(c.manifest.id.clone()),
                        severity: Severity::Error,
                        message: format!(
                            "{} needs `{}` {}, which is not installed",
                            c.manifest.name,
                            dep.id,
                            dep.version.as_str()
                        ),
                    });
                }
                Some(v) if !dep.version.matches(v) => {
                    rejected.insert(c.manifest.id.clone());
                    report.problems.push(Problem {
                        id: Some(c.manifest.id.clone()),
                        severity: Severity::Error,
                        message: format!(
                            "{} needs `{}` {} and the installed one is {v}",
                            c.manifest.name,
                            dep.id,
                            dep.version.as_str()
                        ),
                    });
                }
                Some(_) => {}
            }
        }
    }
    candidates.retain(|c| !rejected.contains(&c.manifest.id));

    // ---- 3. order, and refuse a cycle ------------------------------------
    let (ordered, cyclic) = topo_order(candidates);
    for c in &cyclic {
        report.problems.push(Problem {
            id: Some(c.manifest.id.clone()),
            severity: Severity::Error,
            message: format!(
                "{} is in a dependency cycle — packages that need each other cannot be given a \
                 load order",
                c.manifest.name
            ),
        });
    }
    report.loaded = ordered;
    report
}

/// Kahn's algorithm over the dependency edges. Anything left when no node has
/// an in-degree of zero is in a cycle.
///
/// A dependency that is not in the candidate set has already been rejected in
/// step 2, so edges to it are ignored here rather than treated as unsatisfiable
/// a second time.
fn topo_order(candidates: Vec<Loaded>) -> (Vec<Loaded>, Vec<Loaded>) {
    let present: HashSet<String> = candidates.iter().map(|c| c.manifest.id.clone()).collect();
    let mut indegree: HashMap<String, usize> = candidates
        .iter()
        .map(|c| {
            let n = c.manifest.dependencies.iter().filter(|d| present.contains(&d.id)).count();
            (c.manifest.id.clone(), n)
        })
        .collect();
    // id → who depends on it
    let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
    for c in &candidates {
        for d in &c.manifest.dependencies {
            if present.contains(&d.id) {
                dependents.entry(d.id.clone()).or_default().push(c.manifest.id.clone());
            }
        }
    }

    // Sorted so a project with no dependencies at all still loads in a stable,
    // explainable order rather than in packages.ron's order by accident.
    let mut ready: Vec<String> =
        indegree.iter().filter(|(_, n)| **n == 0).map(|(id, _)| id.clone()).collect();
    ready.sort();
    ready.reverse(); // pop() takes the alphabetically first

    let mut by_id: HashMap<String, Loaded> =
        candidates.into_iter().map(|c| (c.manifest.id.clone(), c)).collect();
    let mut out = Vec::new();
    while let Some(id) = ready.pop() {
        if let Some(l) = by_id.remove(&id) {
            out.push(l);
        }
        let mut freed: Vec<String> = Vec::new();
        for dep in dependents.get(&id).cloned().unwrap_or_default() {
            if let Some(n) = indegree.get_mut(&dep) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    freed.push(dep);
                }
            }
        }
        freed.sort();
        for f in freed.into_iter().rev() {
            ready.push(f);
        }
        ready.sort();
        ready.reverse();
    }
    let mut cyclic: Vec<Loaded> = by_id.into_values().collect();
    cyclic.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
    (out, cyclic)
}

/// Resolve a `pkg://<id>/<path>` reference against the loaded set.
///
/// This is the address that survives a package being linked, copied or moved:
/// `pkg://com.example.grass/textures/blade.png` finds the file wherever that
/// package's folder happens to be. Returns `None` for a malformed reference or
/// an id that is not loaded — the caller reports it, because "which asset" is a
/// question this module cannot answer.
pub fn resolve_pkg_url(loaded: &[Loaded], url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix(PKG_SCHEME)?;
    let (id, rel) = rest.split_once('/')?;
    if rel.is_empty() {
        return None;
    }
    // A `..` here would let one package address another's files — or the
    // project's. The manifest validator refuses them in folder lists for the
    // same reason; this is the runtime half of that rule.
    if Path::new(rel).components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return None;
    }
    let pkg = loaded.iter().find(|l| l.id() == id)?;
    Some(pkg.root.join(rel))
}

/// The scheme that addresses a package's own files.
pub const PKG_SCHEME: &str = "pkg://";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Source;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("flpkg-res-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        floptle_vfs::create_dir_all(&d).unwrap();
        d
    }

    /// Write a package into `<proj>/packages/<id>` and register it.
    fn install(proj: &Path, id: &str, version: &str, body: &str) {
        let root = proj.join("packages").join(id);
        floptle_vfs::create_dir_all(&root).unwrap();
        floptle_vfs::write(
            root.join("package.ron"),
            format!(r#"( id: "{id}", name: "{id}", version: "{version}", {body} )"#),
        )
        .unwrap();
        let mut reg = Registry::load(proj).unwrap();
        reg.upsert(Entry {
            id: id.into(),
            version: version.parse().unwrap(),
            source: Source::Authored,
            enabled: true,
        });
        reg.save(proj).unwrap();
    }

    fn engine() -> Version {
        Version::new(0, 55, 0)
    }

    #[test]
    fn a_project_with_no_packages_resolves_to_nothing() {
        let proj = temp("none");
        let r = resolve(&proj, &engine());
        assert!(r.loaded.is_empty());
        assert!(r.problems.is_empty());
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[test]
    fn dependencies_come_first() {
        let proj = temp("order");
        install(&proj, "com.t.leaf", "1.0.0", "");
        install(
            &proj,
            "com.t.mid",
            "1.0.0",
            r#"dependencies: [ (id: "com.t.leaf", version: "^1.0") ]"#,
        );
        install(
            &proj,
            "com.t.top",
            "1.0.0",
            r#"dependencies: [ (id: "com.t.mid", version: "^1.0") ]"#,
        );
        let r = resolve(&proj, &engine());
        let ids: Vec<&str> = r.loaded.iter().map(|l| l.id()).collect();
        assert_eq!(ids, ["com.t.leaf", "com.t.mid", "com.t.top"], "{:?}", r.problems);
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[test]
    fn a_missing_dependency_skips_only_that_package() {
        let proj = temp("missdep");
        install(&proj, "com.t.fine", "1.0.0", "");
        install(
            &proj,
            "com.t.broken",
            "1.0.0",
            r#"dependencies: [ (id: "com.t.absent", version: "*") ]"#,
        );
        let r = resolve(&proj, &engine());
        assert_eq!(r.loaded.len(), 1);
        assert_eq!(r.loaded[0].id(), "com.t.fine");
        assert!(r.errors().any(|p| p.message.contains("com.t.absent")), "{:?}", r.problems);
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[test]
    fn a_dependency_at_the_wrong_version_names_both_versions() {
        let proj = temp("wrongver");
        install(&proj, "com.t.lib", "1.0.0", "");
        install(
            &proj,
            "com.t.app",
            "1.0.0",
            r#"dependencies: [ (id: "com.t.lib", version: "^2.0") ]"#,
        );
        let r = resolve(&proj, &engine());
        assert_eq!(r.loaded.len(), 1);
        let msg = &r.errors().next().unwrap().message;
        assert!(msg.contains("^2.0") && msg.contains("1.0.0"), "{msg}");
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[test]
    fn a_cycle_is_refused_rather_than_hung() {
        let proj = temp("cycle");
        install(&proj, "com.t.a", "1.0.0", r#"dependencies: [ (id: "com.t.b", version: "*") ]"#);
        install(&proj, "com.t.b", "1.0.0", r#"dependencies: [ (id: "com.t.a", version: "*") ]"#);
        let r = resolve(&proj, &engine());
        assert!(r.loaded.is_empty(), "{:?}", r.loaded.iter().map(|l| l.id()).collect::<Vec<_>>());
        assert_eq!(r.errors().filter(|p| p.message.contains("cycle")).count(), 2);
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[test]
    fn an_engine_requirement_this_build_misses_is_reported_not_loaded() {
        let proj = temp("engver");
        install(&proj, "com.t.future", "1.0.0", r#"engine: ">=9.0.0""#);
        let r = resolve(&proj, &engine());
        assert!(r.loaded.is_empty());
        let msg = &r.errors().next().unwrap().message;
        assert!(msg.contains("9.0.0") && msg.contains("0.55.0"), "{msg}");
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[test]
    fn a_disabled_package_is_listed_and_not_loaded() {
        let proj = temp("off");
        install(&proj, "com.t.a", "1.0.0", "");
        crate::install::set_enabled(&proj, "com.t.a", false).unwrap();
        let r = resolve(&proj, &engine());
        assert!(r.loaded.is_empty());
        assert_eq!(r.disabled.len(), 1);
        assert!(r.problems.is_empty(), "being switched off is not a problem");
        let _ = std::fs::remove_dir_all(&proj);
    }

    /// A broken package must not take the project down with it.
    #[test]
    fn a_broken_manifest_skips_one_package_and_keeps_the_rest() {
        let proj = temp("broken");
        install(&proj, "com.t.good", "1.0.0", "");
        install(&proj, "com.t.bad", "1.0.0", "");
        floptle_vfs::write(proj.join("packages/com.t.bad/package.ron"), "not ron at all {{{").unwrap();
        let r = resolve(&proj, &engine());
        assert_eq!(r.loaded.len(), 1);
        assert_eq!(r.loaded[0].id(), "com.t.good");
        assert_eq!(r.errors().count(), 1);
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[test]
    fn a_version_the_list_disagrees_with_warns_and_still_loads() {
        let proj = temp("drift");
        install(&proj, "com.t.a", "1.0.0", "");
        let mut reg = Registry::load(&proj).unwrap();
        reg.find_mut("com.t.a").unwrap().version = Version::new(0, 1, 0);
        reg.save(&proj).unwrap();
        let r = resolve(&proj, &engine());
        assert_eq!(r.loaded.len(), 1);
        assert_eq!(r.loaded[0].manifest.version, Version::new(1, 0, 0));
        assert_eq!(r.problems.iter().filter(|p| p.severity == Severity::Warning).count(), 1);
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[test]
    fn pkg_urls_find_a_package_wherever_it_lives() {
        let proj = temp("pkgurl");
        install(&proj, "com.t.a", "1.0.0", "");
        let r = resolve(&proj, &engine());
        let got = resolve_pkg_url(&r.loaded, "pkg://com.t.a/textures/blade.png").unwrap();
        assert!(got.ends_with("packages/com.t.a/textures/blade.png"), "{}", got.display());
        assert!(resolve_pkg_url(&r.loaded, "pkg://com.t.absent/x.png").is_none());
        assert!(resolve_pkg_url(&r.loaded, "textures/blade.png").is_none());
        assert!(resolve_pkg_url(&r.loaded, "pkg://com.t.a/").is_none());
        let _ = std::fs::remove_dir_all(&proj);
    }

    /// One package must not be able to address another's files, or the
    /// project's, by climbing out of its own folder.
    #[test]
    fn a_pkg_url_may_not_climb_out() {
        let proj = temp("pkgclimb");
        install(&proj, "com.t.a", "1.0.0", "");
        let r = resolve(&proj, &engine());
        assert!(resolve_pkg_url(&r.loaded, "pkg://com.t.a/../../project.ron").is_none());
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[test]
    fn editor_scripts_are_found_recursively_and_sorted() {
        let proj = temp("edscripts");
        install(&proj, "com.t.a", "1.0.0", "");
        let root = proj.join("packages/com.t.a");
        floptle_vfs::create_dir_all(root.join("editor/sub")).unwrap();
        floptle_vfs::write(root.join("editor/b.lua"), "").unwrap();
        floptle_vfs::write(root.join("editor/a.lua"), "").unwrap();
        floptle_vfs::write(root.join("editor/sub/c.lua"), "").unwrap();
        floptle_vfs::write(root.join("editor/notes.txt"), "").unwrap();
        let r = resolve(&proj, &engine());
        let names: Vec<String> = r.loaded[0]
            .editor_scripts()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a.lua", "b.lua", "c.lua"]);
        let _ = std::fs::remove_dir_all(&proj);
    }
}
