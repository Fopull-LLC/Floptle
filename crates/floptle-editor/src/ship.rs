//! `floptle ship`: a dedicated-server bundle, exported and uploaded to the
//! game's page on Floptle Cloud in one command.
//!
//! The export is `floptle export … server`, unchanged. The upload is
//! [`floptle_account::Account::upload_build`], as the developer signed in to
//! the Hub (the session is shared), in pieces where the site offers them.
//!
//! **Resuming.** A bundle carries the time it was exported, so exporting again
//! makes a different file with a different digest, and the site would start a
//! new build from zero. So the bundle waits in a cache beside a note of the
//! project it came from (how many files, the newest change, their total size).
//! Run again after a kill and, if the project has not changed, the same bundle
//! goes up from where the site's copy ends. If the project has changed, the old
//! bundle is stale and a fresh one is exported. `--fresh` exports regardless.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// What the cached bundle was made from, and what it is.
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
struct Pending {
    project: PathBuf,
    scene: Option<String>,
    label: Option<String>,
    project_stamp: (u64, u64, u64),
    sha256: String,
    size: u64,
    engine: String,
}

/// (files, newest modification in milliseconds, total bytes) under the project,
/// leaving out the editor's own `.floptle` folder, which changes on every open.
/// Any edit that could change the bundle changes one of the three.
pub(crate) fn project_stamp(root: &Path) -> (u64, u64, u64) {
    fn walk(dir: &Path, at_root: bool, acc: &mut (u64, u64, u64)) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let name = e.file_name();
            if at_root && name == ".floptle" {
                continue;
            }
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                walk(&e.path(), false, acc);
            } else {
                acc.0 += 1;
                acc.2 += m.len();
                let t = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                acc.1 = acc.1.max(t);
            }
        }
    }
    let mut acc = (0, 0, 0);
    walk(root, true, &mut acc);
    acc
}

/// Where a game's bundle waits between runs. Outside the project, because an
/// export refuses to write into the project it is exporting.
fn cache_dir(game: &str, project: &Path) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    project.hash(&mut h);
    std::env::temp_dir().join("floptle-ship").join(format!("{game}-{:016x}", h.finish()))
}

fn say_json(v: serde_json::Value) {
    floptle_say::say!("{}", v);
}

/// The verb. Exit 0 shipped, 1 the export or upload failed, 2 not a project,
/// 3 not connected to Cloud or nobody signed in.
pub(crate) fn run(project: &Path, title: &str, scene: Option<&str>, label: Option<&str>, fresh: bool, json: bool) -> i32 {
    let fail = |code: i32, why: String| -> i32 {
        if json {
            say_json(serde_json::json!({ "ok": false, "error": why }));
        } else {
            floptle_say::say_err!("{why}");
        }
        code
    };
    let Ok(proj) = project.canonicalize() else {
        return fail(2, format!("{} is not a project directory", project.display()));
    };
    let cfg = floptle_scene::load_project(&proj.join("project.ron"));
    let Some(game) = cfg.cloud.as_ref().filter(|c| c.is_connected() && !c.game.trim().is_empty()).map(|c| c.game.clone())
    else {
        return fail(
            3,
            "this project is not connected to a game on Floptle Cloud; connect it in \
             ⚙ Settings ▸ Networked (it names the game this bundle belongs to)"
                .into(),
        );
    };

    // Signed in? The Hub's session, read from the keyring.
    let base = std::env::var("FLOPTLE_ACCOUNT_BASE").unwrap_or_else(|_| floptle_account::DEFAULT_BASE.to_string());
    let account = floptle_account::Account::new(base);
    let waited = std::time::Instant::now();
    while account.is_restoring() && waited.elapsed() < std::time::Duration::from_secs(10) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if !account.is_signed_in() {
        return fail(3, "nobody is signed in: sign in to Floptle in the Hub (or the editor) and run this again".into());
    }
    ship_with(&proj, &cfg, &game, title, scene, label, fresh, json, &mut |b, p| account.upload_build(b, p))
}

/// The upload a run makes: the real one is the signed-in account's.
type Upload<'a> = dyn FnMut(
        &floptle_account::builds::Bundle,
        &mut dyn FnMut(floptle_account::builds::Progress) -> bool,
    ) -> Result<String, String>
    + 'a;

/// Everything after the sign-in: find or make the bundle, upload it, and keep
/// it for the next run if the upload does not finish.
#[allow(clippy::too_many_arguments)]
fn ship_with(
    proj: &Path,
    cfg: &floptle_scene::ProjectConfigDoc,
    game: &str,
    title: &str,
    scene: Option<&str>,
    label: Option<&str>,
    fresh: bool,
    json: bool,
    upload: &mut Upload,
) -> i32 {
    let fail = |code: i32, why: String| -> i32 {
        if json {
            say_json(serde_json::json!({ "ok": false, "error": why }));
        } else {
            floptle_say::say_err!("{why}");
        }
        code
    };
    let proj = proj.to_path_buf();
    let dir = cache_dir(game, &proj);
    let bundle = dir.join(format!("{game}-server.tar.gz"));
    let note = dir.join("ship.json");
    let stamp = project_stamp(&proj);
    let want = |p: &Pending| {
        p.project == proj && p.scene.as_deref() == scene && p.label.as_deref() == label && p.project_stamp == stamp
    };
    let previous: Option<Pending> = std::fs::read_to_string(&note).ok().and_then(|s| serde_json::from_str(&s).ok());
    let pending = match previous.filter(|p| !fresh && want(p) && floptle_vfs::size(&bundle) == Some(p.size)) {
        Some(p) => {
            if !json {
                floptle_say::say!("the project has not changed since the last try: carrying on with that bundle");
            }
            p
        }
        None => {
            let _ = std::fs::remove_dir_all(&dir);
            if let Err(e) = std::fs::create_dir_all(&dir) {
                return fail(1, format!("make {}: {e}", dir.display()));
            }
            match crate::export::export_server(&proj, &bundle, title, scene, label) {
                Ok((msg, _)) => {
                    if !json {
                        floptle_say::say!("{msg}");
                    }
                }
                Err(e) => return fail(1, e),
            }
            let size = floptle_vfs::size(&bundle).unwrap_or(0);
            let sha256 = match floptle_account::builds::sha256_file(&bundle) {
                Ok(s) => s,
                Err(e) => return fail(1, e),
            };
            let engine = cfg.engine_version.clone().unwrap_or_else(crate::distribution_version);
            let p = Pending {
                project: proj.clone(),
                scene: scene.map(str::to_string),
                label: label.map(str::to_string),
                project_stamp: stamp,
                sha256,
                size,
                engine,
            };
            let _ = std::fs::write(&note, serde_json::to_string(&p).unwrap_or_default());
            p
        }
    };

    let b = floptle_account::builds::Bundle {
        game,
        path: &bundle,
        sha256: &pending.sha256,
        size: pending.size,
        engine_version: &pending.engine,
        label,
    };
    let mb = |n: u64| n as f64 / (1024.0 * 1024.0);
    let mut last_tenth = u64::MAX;
    let shipped = upload(&b, &mut |p| {
        if json {
            return true;
        }
        match p {
            floptle_account::builds::Progress::Reserved { build_id, already } if already > 0 => {
                floptle_say::say!("build {build_id}: {:.1} MB already on fopull.com, sending the rest", mb(already));
            }
            floptle_account::builds::Progress::Reserved { build_id, .. } => {
                floptle_say::say!("build {build_id}: uploading {:.1} MB", mb(pending.size));
            }
            floptle_account::builds::Progress::Sent { sent, total } => {
                // A line per tenth, not per piece: a 200 MB bundle is 25 pieces.
                let tenth = sent * 10 / total.max(1);
                if tenth != last_tenth {
                    last_tenth = tenth;
                    floptle_say::say!("  {:.1} / {:.1} MB", mb(sent), mb(total));
                }
            }
            floptle_account::builds::Progress::Completing => floptle_say::say!("checking the digest…"),
        }
        true
    });
    match shipped {
        Ok(build) => {
            let _ = std::fs::remove_dir_all(&dir);
            if json {
                say_json(serde_json::json!({
                    "ok": true, "build": build, "game": game, "bytes": pending.size,
                    "sha256": pending.sha256, "engine": pending.engine,
                }));
            } else {
                floptle_say::say!(
                    "shipped build {build} to {game} ({:.1} MB, engine {}). Deploy it from \
                     https://fopull.com/cloud/games/{game}",
                    mb(pending.size),
                    pending.engine
                );
            }
            0
        }
        Err(e) => fail(1, format!("{e}\n  the bundle is kept, so running the same command again carries on")),
    }
}

/// A `floptle ship` started from ⚙ Settings ▸ Networked: the same verb, run as a
/// child of this binary so the editor never blocks on an upload, with its
/// lines fed to the Console as they come.
pub(crate) struct ShipJob {
    child: std::process::Child,
    lines: std::sync::mpsc::Receiver<(bool, String)>,
}

impl crate::Editor {
    pub(crate) fn start_ship(&mut self) {
        if self.ship_job.is_some() {
            return;
        }
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                self.console.push(floptle_script::LogLevel::Error, format!("⬆ ship: cannot find this program: {e}"), None);
                return;
            }
        };
        let spawned = std::process::Command::new(exe)
            .arg("ship")
            .arg(&self.project_root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                self.console.push(floptle_script::LogLevel::Error, format!("⬆ ship: could not start: {e}"), None);
                return;
            }
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let pipes: [(bool, Option<Box<dyn std::io::Read + Send>>); 2] = [
            (false, child.stdout.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>)),
            (true, child.stderr.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>)),
        ];
        for (is_err, pipe) in pipes {
            let Some(pipe) = pipe else { continue };
            let tx = tx.clone();
            crate::worker::spawn("floptle-ship-output", move || {
                use std::io::BufRead as _;
                for line in std::io::BufReader::new(pipe).lines().map_while(Result::ok) {
                    if tx.send((is_err, line)).is_err() {
                        break;
                    }
                }
            });
        }
        self.console.push(floptle_script::LogLevel::Debug, "⬆ shipping a server build to Floptle Cloud…".into(), None);
        self.ship_job = Some(ShipJob { child, lines: rx });
    }

    /// Once a frame: the job's new lines to the Console, and its ending.
    pub(crate) fn poll_ship(&mut self) {
        let Some(job) = self.ship_job.as_mut() else { return };
        let lines: Vec<(bool, String)> = job.lines.try_iter().collect();
        let status = job.child.try_wait();
        for (is_err, line) in lines {
            let level = if is_err { floptle_script::LogLevel::Error } else { floptle_script::LogLevel::Debug };
            self.console.push(level, format!("⬆ {line}"), None);
        }
        match status {
            Ok(None) => {}
            Ok(Some(s)) => {
                // The last lines may still be in the pipe: take them before
                // saying how it ended.
                if let Some(job) = self.ship_job.take() {
                    for (is_err, line) in job.lines.try_iter() {
                        let level = if is_err { floptle_script::LogLevel::Error } else { floptle_script::LogLevel::Debug };
                        self.console.push(level, format!("⬆ {line}"), None);
                    }
                }
                if !s.success() {
                    self.console.push(
                        floptle_script::LogLevel::Warn,
                        "⬆ the server build was not shipped; the lines above say why".into(),
                        None,
                    );
                }
            }
            Err(e) => {
                self.ship_job = None;
                self.console.push(floptle_script::LogLevel::Error, format!("⬆ ship: {e}"), None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_account::builds::{Bundle, Progress};

    const SERVABLE: &str = "(entry_scene: Some(\"first\"), engine_version: Some(\"0.85.0\"), \
                            cloud: Some((game: \"shiptest\", key: \"fk_live_T\")))";

    fn servable(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("floptle-ship-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("scenes")).unwrap();
        std::fs::write(root.join("project.ron"), SERVABLE).unwrap();
        std::fs::write(root.join("scenes/first.ron"), "(nodes: [(name: \"Player\", net: Some((predicted: true)))])")
            .unwrap();
        root.canonicalize().unwrap()
    }

    /// **A killed upload carries on with the same bundle; an edited project
    /// gets a new one.** A bundle carries its export time, so exporting again
    /// makes a different digest and the site would start a new build from
    /// zero. The digest the second run uploads is the first run's.
    #[test]
    fn a_second_run_uploads_the_same_bundle_until_the_project_changes() {
        let proj = servable("resume");
        let cfg = floptle_scene::load_project(&proj.join("project.ron"));
        let mut digests: Vec<String> = Vec::new();
        let attempt = |ok: bool, digests: &mut Vec<String>| {
            ship_with(&proj, &cfg, "shiptest", "T", None, None, false, true, &mut |b: &Bundle, _p: &mut dyn FnMut(Progress) -> bool| {
                digests.push(b.sha256.to_string());
                if ok { Ok("b_1".into()) } else { Err("the connection dropped".into()) }
            })
        };
        assert_eq!(attempt(false, &mut digests), 1, "a failed upload must exit 1");
        assert!(cache_dir("shiptest", &proj).join("ship.json").exists(), "the unfinished bundle was not kept");
        // Exports a second apart differ (they carry the time), so waiting
        // proves the second run did not export again.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert_eq!(attempt(true, &mut digests), 0);
        assert_eq!(digests[0], digests[1], "the second run exported a new bundle instead of carrying on");
        assert!(!cache_dir("shiptest", &proj).exists(), "a finished upload left its bundle behind");

        // After an edit, a new bundle: the old one is stale.
        assert_eq!(attempt(false, &mut digests), 1);
        std::fs::write(proj.join("scenes/first.ron"), "(nodes: [(name: \"Player2\", net: Some((predicted: true)))])").unwrap();
        assert_eq!(attempt(true, &mut digests), 0);
        assert_ne!(digests[2], digests[3], "an edited project uploaded the stale bundle");
        let _ = std::fs::remove_dir_all(&proj);
    }

    /// Any edit that could change the bundle changes the stamp; opening the
    /// project in the editor (which rewrites `.floptle`) does not.
    #[test]
    fn the_stamp_moves_with_the_project_and_not_with_the_editors_own_folder() {
        let root = std::env::temp_dir().join(format!("floptle-ship-stamp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::create_dir_all(root.join(".floptle/library")).unwrap();
        std::fs::write(root.join("scripts/a.lua"), "x = 1").unwrap();
        let before = project_stamp(&root);

        std::fs::write(root.join(".floptle/library/floptle.lua"), "-- regenerated").unwrap();
        assert_eq!(project_stamp(&root), before, "the editor's own folder moved the stamp");

        std::fs::write(root.join("scripts/a.lua"), "x = 22").unwrap();
        assert_ne!(project_stamp(&root), before, "an edit to a script did not move the stamp");
        let edited = project_stamp(&root);
        std::fs::write(root.join("scripts/b.lua"), "").unwrap();
        assert_ne!(project_stamp(&root), edited, "a new file did not move the stamp");
        let _ = std::fs::remove_dir_all(&root);
    }
}
