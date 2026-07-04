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

/// Launch the editor for `project` using `install`, detached — the Hub keeps running.
/// Errors (without spawning) if the install is missing its binary or the project is gone.
pub fn launch(install: &Install, project: &Project) -> Result<(), String> {
    if !install.is_valid() {
        return Err(format!("engine {} is missing its editor binary", install.version));
    }
    if !project.exists() {
        return Err(format!("project folder is gone: {}", project.path.display()));
    }
    launch_command(install, &project.path)
        .current_dir(&project.path)
        .spawn()
        .map(|_child| ())
        .map_err(|e| format!("could not launch the editor: {e}"))
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
    fn launch_refuses_invalid_install_or_missing_project() {
        let tmp = tempfile::tempdir().unwrap();
        let bad_install = Install { version: "0.1.0".into(), path: tmp.path().join("nope") };
        let project = Project {
            name: "P".into(),
            path: tmp.path().to_path_buf(),
            engine_version: None,
            last_opened: None,
        };
        assert!(launch(&bad_install, &project).is_err(), "no editor binary");
    }
}
