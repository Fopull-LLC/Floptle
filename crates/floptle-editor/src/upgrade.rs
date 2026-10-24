//! The upgrade pass: what moving a project onto a newer engine does to it.
//!
//! The rule is that an upgraded project keeps looking and behaving as it did.
//! So every change to a look or a behaviour that would otherwise reach an
//! existing project ships with a step here: a migration keyed on the version
//! it arrived in. A new project starts with the new defaults and needs none.
//!
//! [`run`] runs every step that arrived after the project's stamped engine
//! version, in the order [`STEPS`] lists them, and reports the files each one
//! changed. It is run by `floptle migrate` (what the Hub's upgrade calls) and
//! when the editor opens a project stamped with an older engine; both then
//! stamp the project with the engine that ran it, so no step runs twice.
//!
//! **A step must be safe to run on any project, any number of times.** It
//! writes a file only when the file needs changing, and leaves it byte for
//! byte otherwise, so a step that finds nothing to do changes nothing (not
//! even a timestamp a version-control tool would notice). A project that is
//! already current, or that a step does not apply to, is the common case.
//!
//! **Adding a step:** write the function in your own module (signature
//! [`StepFn`]), and add one line to [`STEPS`], at the end, with the version
//! the change ships in. Steps run in list order, so a later step can rely on
//! an earlier one having run.

use std::path::{Path, PathBuf};

/// What a step does: given the project root and the version it was stamped
/// with (`None` for a project from before stamps), bring the project up to
/// date and return the files it wrote. An `Err` is reported and the pass goes
/// on; the project is still stamped, so the step is not retried forever.
pub(crate) type StepFn = fn(&Path, Option<&str>) -> Result<Vec<PathBuf>, String>;

/// One migration.
pub(crate) struct Step {
    /// A short stable name, for the log and for tests.
    pub(crate) id: &'static str,
    /// The engine version the change it migrates shipped in. The step runs for
    /// a project stamped older than this.
    pub(crate) since: &'static str,
    pub(crate) run: StepFn,
}

/// Every step, oldest first. Register a new one with a line at the end.
pub(crate) const STEPS: &[Step] = &[
    Step { id: "effects-format", since: "0.117.0", run: effects_format },
    Step { id: "ignore-generated-library", since: "0.117.0", run: ignore_generated_library },
    Step { id: "terrain-generator", since: "0.117.0", run: crate::terrain_upgrade::pin_terrain_generator },
    Step { id: "body-sizes-in-node-space", since: "0.117.0", run: crate::body_upgrade::keep_world_sizes },
    Step { id: "model-look-and-blend", since: "0.117.0", run: crate::upgrade_models::keep_model_look },
];

/// Is `version` older than `since`? A missing stamp is older than everything.
pub(crate) fn older(version: Option<&str>, since: &str) -> bool {
    version.is_none_or(|v| floptle_dist::version_key(v) < floptle_dist::version_key(since))
}

/// The steps a project stamped `from` needs, in order.
pub(crate) fn pending<'a>(steps: &'a [Step], from: Option<&str>) -> Vec<&'a Step> {
    steps.iter().filter(|s| older(from, s.since)).collect()
}

/// What one step did.
pub(crate) struct Done {
    pub(crate) id: &'static str,
    pub(crate) result: Result<Vec<PathBuf>, String>,
}

/// Run every step `root` needs, given its stamp `from`, in order. Does not
/// stamp: the caller does, once, after the pass.
pub(crate) fn run(root: &Path, from: Option<&str>) -> Vec<Done> {
    run_steps(STEPS, root, from)
}

pub(crate) fn run_steps(steps: &[Step], root: &Path, from: Option<&str>) -> Vec<Done> {
    pending(steps, from).into_iter().map(|s| Done { id: s.id, result: (s.run)(root, from) }).collect()
}

/// The lines a pass says, one per file changed and one per failure. The
/// `rewrote <path> (…)` form is what the Hub's upgrade summary reads.
pub(crate) fn report(root: &Path, done: &[Done]) -> Vec<String> {
    let mut out = Vec::new();
    for d in done {
        match &d.result {
            Ok(files) => {
                for f in files {
                    let shown = f.strip_prefix(root).unwrap_or(f);
                    out.push(format!("rewrote {} (upgrade step {})", shown.display(), d.id));
                }
            }
            Err(e) => out.push(format!("upgrade step {} failed: {e}", d.id)),
        }
    }
    out
}

impl crate::Editor {
    /// Opening a project stamped with an older engine runs the upgrade pass
    /// and stamps it, as `floptle migrate` does, and says in the Console what
    /// it changed. A project without a `project.ron` is not a project anything
    /// stamped, and is left alone; one already current costs one comparison.
    pub(crate) fn upgrade_older_project(&mut self) {
        #[cfg(test)]
        if let Some(engine) = self.upgrade_to_for_test.clone() {
            return self.upgrade_older_project_to(&engine);
        }
        let engine = crate::distribution_version();
        // An in-workspace build has no release version to upgrade to (it
        // reports 0.0.0), and must not stamp one onto the projects it opens.
        if engine != "0.0.0" {
            self.upgrade_older_project_to(&engine);
        }
    }

    /// [`Self::upgrade_older_project`] onto engine version `engine`.
    pub(crate) fn upgrade_older_project_to(&mut self, engine: &str) {
        let engine = engine.to_string();
        let cfg_path = self.project_cfg_path();
        if !floptle_vfs::exists(&cfg_path) {
            return;
        }
        let from = self.project.engine_version.clone();
        if !older(from.as_deref(), &engine) {
            return;
        }
        let done = run(&self.project_root, from.as_deref());
        for line in report(&self.project_root, &done) {
            self.console.push(floptle_script::LogLevel::Debug, line, None);
        }
        // Stamp the file as it is on disk, so nothing else in it moves.
        if let Ok(Some(mut cfg)) = floptle_scene::try_load_project(&cfg_path) {
            cfg.engine_version = Some(engine.clone());
            let _ = floptle_scene::save_project(&cfg, &cfg_path);
            // Every setting a step wrote is the open project's too, or its
            // next save of project.ron would write them away.
            self.project = cfg;
        }
        self.project.engine_version = Some(engine);
    }
}

// ---------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------

/// Every `.vfx.ron` brought up to the current effect format: the legacy
/// track-level rate and bursts folded into clips, a scalar spin into a vector.
/// An effect already current is not written.
fn effects_format(root: &Path, _from: Option<&str>) -> Result<Vec<PathBuf>, String> {
    let mut changed = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = floptle_vfs::read_dir(&dir) else { continue };
        for entry in rd {
            let p = entry.path();
            if entry.is_dir() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.starts_with('.') && name != "target" {
                    stack.push(p);
                }
            } else if p.to_string_lossy().ends_with(floptle_scene::VFX_EXT) {
                match floptle_scene::migrate_vfx_effect_file(&p) {
                    Ok(true) => changed.push(p),
                    Ok(false) => {}
                    // One effect that will not parse is the author's to fix;
                    // it must not stop the rest of the project upgrading.
                    Err(e) => floptle_say::say_err!("left {} untouched (won't parse: {e})", p.display()),
                }
            }
        }
    }
    Ok(changed)
}

/// The line that keeps the engine's generated scripting stubs out of a
/// game's version control.
pub(crate) const LIBRARY_IGNORE: &str = ".floptle/library/";

/// `.floptle/library/` holds the annotations the editor writes from the
/// installed engine each time it opens the project. They belong to the
/// engine, not the game, so `.gitignore` names them: added if the file lacks
/// it, the file created if there is none, and left alone otherwise.
pub(crate) fn ignore_generated_library(root: &Path, _from: Option<&str>) -> Result<Vec<PathBuf>, String> {
    let path = root.join(".gitignore");
    let cur = floptle_vfs::read_to_string(&path).unwrap_or_default();
    let has = cur.lines().any(|l| {
        let l = l.trim();
        l == LIBRARY_IGNORE || l == ".floptle/library" || l == "/.floptle/library/" || l == "/.floptle/library"
    });
    if has {
        return Ok(Vec::new());
    }
    let mut next = cur;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str("# Scripting annotations the engine writes when it opens the project.\n");
    next.push_str(LIBRARY_IGNORE);
    next.push('\n');
    floptle_vfs::write(&path, next).map_err(|e| e.to_string())?;
    Ok(vec![path])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        static ORDER: std::cell::RefCell<Vec<&'static str>> = const { std::cell::RefCell::new(Vec::new()) };
        static RUNS: Cell<u32> = const { Cell::new(0) };
    }

    fn first(_: &Path, _: Option<&str>) -> Result<Vec<PathBuf>, String> {
        ORDER.with(|o| o.borrow_mut().push("first"));
        RUNS.with(|r| r.set(r.get() + 1));
        Ok(Vec::new())
    }
    fn second(_: &Path, _: Option<&str>) -> Result<Vec<PathBuf>, String> {
        ORDER.with(|o| o.borrow_mut().push("second"));
        Err("nope".into())
    }
    const TEST_STEPS: &[Step] = &[
        Step { id: "first", since: "0.110.0", run: first },
        Step { id: "second", since: "0.117.0-rc3", run: second },
    ];

    /// **A step runs for an older stamp, in order, and not again once the
    /// project is stamped past it.** A pre-release counts as older than its
    /// release, and a project from before stamps needs every step.
    #[test]
    fn steps_run_in_order_and_only_for_older_stamps() {
        let root = Path::new("/nonexistent");
        let ids = |from: Option<&str>| pending(TEST_STEPS, from).iter().map(|s| s.id).collect::<Vec<_>>();
        assert_eq!(ids(None), ["first", "second"]);
        assert_eq!(ids(Some("0.100.0")), ["first", "second"]);
        assert_eq!(ids(Some("0.110.0")), ["second"], "a project stamped at a step's own version has it");
        assert_eq!(ids(Some("0.117.0-rc2")), ["second"]);
        assert!(ids(Some("0.117.0-rc3")).is_empty());
        assert!(ids(Some("0.117.0")).is_empty());

        ORDER.with(|o| o.borrow_mut().clear());
        RUNS.with(|r| r.set(0));
        let done = run_steps(TEST_STEPS, root, Some("0.100.0"));
        assert_eq!(ORDER.with(|o| o.borrow().clone()), ["first", "second"]);
        assert_eq!(report(root, &done), ["upgrade step second failed: nope"], "a failure is said, and the pass went on");
        // Stamped by the pass, the next upgrade runs nothing.
        run_steps(TEST_STEPS, root, Some("0.117.0"));
        assert_eq!(RUNS.with(Cell::get), 1, "a step ran twice");
    }

    /// **Opening an older project upgrades it, once.** The editor runs the
    /// steps a project stamped 0.100.0 needs, says what it changed, and stamps
    /// it, so the next open runs nothing and changes nothing.
    #[test]
    fn opening_an_older_project_upgrades_it_once() {
        let dir = std::env::temp_dir().join(format!("floptle_upgrade_open_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("project.ron"), "(engine_version: Some(\"0.100.0\"))").unwrap();
        let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
        ed.project = ed.read_project_config();
        ed.upgrade_older_project_to("0.118.0");
        let said: Vec<String> = ed.console.entries.iter().map(|e| e.msg.clone()).collect();
        assert!(said.iter().any(|m| m.contains("rewrote .gitignore")), "{said:?}");
        assert_eq!(floptle_scene::load_project(&dir.join("project.ron")).engine_version.as_deref(), Some("0.118.0"));
        let ignore = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        let project = std::fs::read_to_string(dir.join("project.ron")).unwrap();
        ed.console.entries.clear();
        ed.upgrade_older_project_to("0.118.0");
        assert!(ed.console.entries.is_empty(), "a current project was upgraded again");
        assert_eq!(std::fs::read_to_string(dir.join(".gitignore")).unwrap(), ignore);
        assert_eq!(std::fs::read_to_string(dir.join("project.ron")).unwrap(), project);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **An older project is upgraded before its scene is loaded.** Opening
    /// it loaded the scene first and upgraded after, so the scene in memory
    /// held the old sizes, and its next save wrote them back over the
    /// upgraded file under a stamp that never upgrades again: a scaled ball
    /// came back twice the size.
    #[test]
    fn an_older_project_opens_with_its_scene_already_upgraded() {
        let dir = std::env::temp_dir().join(format!("floptle_upgrade_scene_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("scenes")).unwrap();
        std::fs::write(
            dir.join("project.ron"),
            "(engine_version: Some(\"0.116.0\"), entry_scene: Some(\"scenes/first.ron\"))",
        )
        .unwrap();
        std::fs::write(
            dir.join("scenes/first.ron"),
            r#"(name: "first", nodes: [(
                name: "Ball",
                transform: (translation: (0.0, 5.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (2.0, 2.0, 2.0)),
                matter: Primitive(shape: Sphere, color: (1.0, 1.0, 1.0)),
                scripts: [],
                rigidbody: Some((radius: 0.9)),
            )])"#,
        )
        .unwrap();
        let mut ed = crate::Editor { upgrade_to_for_test: Some("0.117.0".into()), ..Default::default() };
        ed.open_project(dir.clone());
        let (e, rb) = ed
            .world
            .query::<floptle_core::RigidBody>()
            .map(|(e, rb)| (e, *rb))
            .next()
            .expect("the ball has its body");
        let scale = floptle_core::world_transform(&ed.world, e).scale;
        assert!(
            (rb.world_radius(scale) - 0.9).abs() < 1e-5,
            "the open scene holds the pre-upgrade size: radius {} at scale {scale:?}",
            rb.world_radius(scale)
        );
        assert_eq!(ed.project.engine_version.as_deref(), Some("0.117.0"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A step that finds nothing to do leaves every byte alone, and a second
    /// run of one that did something changes nothing more.** The real steps,
    /// on a project that is already current and then on one that is not.
    #[test]
    fn the_shipped_steps_are_idempotent_and_leave_current_files_alone() {
        let dir = std::env::temp_dir().join(format!("floptle_upgrade_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("vfx")).unwrap();
        let effect = "// a comment\n(name: \"Spark\", tracks: [(name: \"A\", clips: [(start: 0.0, end: 0.2, emit: Some(Rate(rate: 40.0)))])])\n";
        std::fs::write(dir.join("vfx/Spark.vfx.ron"), effect).unwrap();
        let ignore = "target/\n# mine\n.floptle/library/\n";
        std::fs::write(dir.join(".gitignore"), ignore).unwrap();
        let done = run(&dir, None);
        assert!(done.iter().all(|d| d.result.as_ref().is_ok_and(Vec::is_empty)), "a current project was changed: {:?}", report(&dir, &done));
        assert_eq!(std::fs::read_to_string(dir.join("vfx/Spark.vfx.ron")).unwrap(), effect);
        assert_eq!(std::fs::read_to_string(dir.join(".gitignore")).unwrap(), ignore);

        // A .gitignore without the line gets it once, appended after what was there.
        std::fs::write(dir.join(".gitignore"), "target/").unwrap();
        let done = run(&dir, None);
        assert_eq!(report(&dir, &done), ["rewrote .gitignore (upgrade step ignore-generated-library)"]);
        let once = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(once.starts_with("target/\n") && once.ends_with(".floptle/library/\n"), "{once:?}");
        assert!(run(&dir, None).iter().all(|d| d.result.as_ref().is_ok_and(Vec::is_empty)), "the second run changed something");
        assert_eq!(std::fs::read_to_string(dir.join(".gitignore")).unwrap(), once);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
