//! Launching the editor for a project, per-OS.

use crate::registry::{Install, Project};
use std::path::Path;
use std::process::Command;

/// The command that launches `install`'s editor with `project_path` as its positional arg
/// (the path the editor opens). macOS opens a `Floptle.app` bundle if present (via `open
/// -a … --args`), else the flat binary; Windows/Linux run the binary directly.
pub fn launch_command(install: &Install, project_path: &Path) -> Command {
    #[cfg(target_os = "macos")]
    {
        let app = install.path.join("Floptle.app");
        if app.is_dir() {
            let mut c = Command::new("open");
            c.arg("-a").arg(&app).arg("--args").arg(project_path);
            return c;
        }
    }
    let mut c = Command::new(install.editor_bin());
    c.arg(project_path);
    c
}

/// The OS command that opens `path` in the system file manager (Explorer / Finder / the
/// user's `xdg-open` handler). Pure construction so it stays unit-testable.
pub fn reveal_command(path: &Path) -> Command {
    let program = if cfg!(target_os = "windows") {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut c = Command::new(program);
    c.arg(path);
    c
}

/// Open `path` in the system file manager, detached. Best-effort — returns an error string
/// if the opener can't be spawned (e.g. no `xdg-open` installed).
pub fn reveal(path: &Path) -> Result<(), String> {
    let child = reveal_command(path)
        .spawn()
        .map_err(|e| format!("could not open {}: {e}", path.display()))?;
    // Reap the (usually short-lived) opener on a detached thread so a long Hub session
    // doesn't accumulate zombies on unix — same as launch().
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(())
}

/// How many editor logs are kept, and how large one may grow before the
/// editor is pointed at a fresh one instead.
pub const KEEP_LOGS: usize = 5;
pub const MAX_LOG_BYTES: u64 = 20 * 1024 * 1024;

/// Where the editor's terminal output goes when the Hub launches it.
///
/// **The editor inherited the Hub's stdout and stderr**, and the Hub is a
/// desktop app: those descriptors are whatever the desktop handed it — often a
/// pipe nobody reads, or a terminal that has since closed. A write to a dead
/// descriptor fails, and the editor's Console mirror wrote there on every
/// line. Its own file removes that case entirely, and gives support something
/// to ask for. `None` when the directory cannot be made; the caller then hands
/// the editor `/dev/null`-shaped handles rather than the Hub's.
pub fn editor_log_file(logs_dir: &Path) -> Option<std::fs::File> {
    std::fs::create_dir_all(logs_dir).ok()?;
    prune_logs(logs_dir);
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut path = logs_dir.join(format!("editor-{stamp}.log"));
    // Two launches in one second share a stamp; the second gets a suffix
    // rather than the first's file.
    let mut n = 1;
    while path.exists() {
        path = logs_dir.join(format!("editor-{stamp}-{n}.log"));
        n += 1;
    }
    std::fs::File::create(&path).ok()
}

/// Keep the newest [`KEEP_LOGS`] editor logs and drop the rest, oldest first.
/// A log past [`MAX_LOG_BYTES`] is dropped too — it is a run that printed
/// something every frame for hours, and nothing in it is worth its disk.
pub fn prune_logs(logs_dir: &Path) {
    let Ok(rd) = std::fs::read_dir(logs_dir) else { return };
    let mut logs: Vec<(std::path::PathBuf, std::time::SystemTime, u64)> = rd
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let name = p.file_name()?.to_str()?;
            if !(name.starts_with("editor-") && name.ends_with(".log")) {
                return None;
            }
            let m = e.metadata().ok()?;
            Some((p, m.modified().ok()?, m.len()))
        })
        .collect();
    logs.sort_by_key(|l| std::cmp::Reverse(l.1));
    // Oversized ones go first, then everything past the count — so a huge log
    // does not also cost a small one its place.
    let mut kept = 0;
    for (p, _, len) in &logs {
        if *len > MAX_LOG_BYTES || kept + 1 >= KEEP_LOGS {
            let _ = std::fs::remove_file(p);
        } else {
            kept += 1;
        }
    }
}

/// Launch the editor for `project` using `install`, detached — the Hub keeps running.
/// Errors (without spawning) if the install is missing its binary or the project is gone.
///
/// The editor's stdout and stderr go to a file under `logs_dir` (see
/// [`editor_log_file`]), or nowhere at all — never to the Hub's own, which
/// may be a descriptor nothing is reading.
pub fn launch(install: &Install, project: &Project, logs_dir: &Path) -> Result<(), String> {
    if !install.is_valid() {
        return Err(format!("engine {} is missing its editor binary", install.version));
    }
    if !project.exists() {
        return Err(format!("project folder is gone: {}", project.path.display()));
    }
    let (out, err) = match editor_log_file(logs_dir) {
        Some(f) => {
            let e = f.try_clone().map(std::process::Stdio::from).unwrap_or_else(|_| std::process::Stdio::null());
            (std::process::Stdio::from(f), e)
        }
        None => (std::process::Stdio::null(), std::process::Stdio::null()),
    };
    let child = launch_command(install, &project.path)
        .current_dir(&project.path)
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
        .map_err(|e| format!("could not launch the editor: {e}"))?;
    // Reap the child on a detached thread so a long Hub session doesn't accumulate zombies
    // on unix (the editor otherwise outlives the Hub's attention but not its process table).
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn launch_command_targets_the_editor_and_project() {
        let install = Install { version: "0.3.0".into(), path: PathBuf::from("/opt/floptle/0.3.0") };
        let project = PathBuf::from("/home/ty/games/mygame");
        let cmd = launch_command(&install, &project);
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        // The project path is always passed as an argument, on every platform.
        assert!(args.iter().any(|a| a == "/home/ty/games/mygame"), "args were {args:?}");
        // On non-macOS the program IS the editor binary.
        #[cfg(not(target_os = "macos"))]
        assert!(
            cmd.get_program().to_string_lossy().ends_with(crate::registry::editor_bin_name()),
            "program was {:?}",
            cmd.get_program()
        );
    }

    #[test]
    fn reveal_command_targets_the_path() {
        let p = PathBuf::from("/home/ty/games/mygame");
        let cmd = reveal_command(&p);
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, ["/home/ty/games/mygame"]);
    }

    #[test]
    fn launch_refuses_invalid_install_or_missing_project() {
        let tmp = tempfile::tempdir().unwrap();
        let bad_install = Install { version: "0.1.0".into(), path: tmp.path().join("nope") };
        let project = Project {
            name: "P".into(),
            path: tmp.path().to_path_buf(),
            engine_version: None,
            last_opened: None,
        };
        assert!(launch(&bad_install, &project, &tmp.path().join("logs")).is_err(), "no editor binary");
    }

    /// **The editor's output lands in a file the Hub owns, and the files do not
    /// pile up.** Five are kept, newest first; one over the size cap goes
    /// whatever its age. The launch itself is exercised through a real child —
    /// a shell that writes to both descriptors — so "redirected" is a fact
    /// about the spawned process and not about a `Command` nobody ran.
    #[cfg(unix)]
    #[test]
    fn the_editor_writes_to_a_log_file_and_old_logs_are_pruned() {
        let tmp = tempfile::tempdir().unwrap();
        let logs = tmp.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        // Seven old logs, one of them huge; the newest four small ones survive
        // alongside the file the new launch creates.
        for i in 0..7 {
            let p = logs.join(format!("editor-{}.log", 1_000 + i));
            std::fs::write(&p, "old").unwrap();
            let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000 + i);
            std::fs::File::options().write(true).open(&p).unwrap().set_modified(t).unwrap();
        }
        let huge = logs.join("editor-1006.log");
        std::fs::File::create(&huge).unwrap().set_len(MAX_LOG_BYTES + 1).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&huge)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_006))
            .unwrap();

        // A fake "editor": a script that prints to both descriptors.
        let install_dir = tmp.path().join("engine");
        std::fs::create_dir_all(&install_dir).unwrap();
        let bin = install_dir.join(crate::registry::editor_bin_name());
        std::fs::write(&bin, "#!/bin/sh\necho out-line\necho err-line >&2\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let install = Install { version: "0.0.0".into(), path: install_dir };
        let project = Project {
            name: "P".into(),
            path: tmp.path().to_path_buf(),
            engine_version: None,
            last_opened: None,
        };
        launch(&install, &project, &logs).expect("launches");
        // The child is reaped on a thread; give it a moment to run.
        std::thread::sleep(std::time::Duration::from_millis(300));

        let mut names: Vec<String> = std::fs::read_dir(&logs)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), KEEP_LOGS, "{names:?}");
        assert!(!names.contains(&"editor-1006.log".to_string()), "the oversized log survived: {names:?}");
        assert!(!names.contains(&"editor-1000.log".to_string()), "the oldest log survived: {names:?}");
        assert!(names.contains(&"editor-1005.log".to_string()), "a recent log was pruned: {names:?}");
        let newest = names.iter().find(|n| !n.starts_with("editor-100")).expect("the new log");
        let text = std::fs::read_to_string(logs.join(newest)).unwrap();
        assert!(text.contains("out-line") && text.contains("err-line"), "{text:?}");
    }
}
