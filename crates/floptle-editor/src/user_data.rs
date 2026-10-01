//! Where an exported game keeps a player's data.
//!
//! A build used to write its saves into its own folder, so every new build
//! started with none: unzipped beside the old one, it looked in its own empty
//! `save/`; installed somewhere read-only, it could not save at all. An
//! exported game now keeps the player's data in a per-user folder named by
//! the project's `data_id`, which does not change between versions:
//!
//! | OS | folder |
//! |---|---|
//! | Windows | `%APPDATA%\<studio>\<data_id>\` (no `<studio>` when unset) |
//! | macOS | `~/Library/Application Support/<data_id>/` |
//! | Linux | `$XDG_DATA_HOME/<data_id>/` (`~/.local/share/<data_id>/`) |
//!
//! Inside it: `save/` (the `save.*` slots and `crash.txt`), `replays/`, and
//! `user/` (everything a script writes under `user://`). `FLOPTLE_DATA_DIR`
//! overrides the folder, for a portable install or a test.
//!
//! Only an exported build moves. The editor, `floptle run` / `shot` / `play`
//! and a browser build keep all of it in the project (or the page's storage),
//! so development and tests stay self-contained. See `floptle_script::paths`
//! for how scripts address it.

use std::path::{Path, PathBuf};

/// The environment variable that overrides the data folder.
pub(crate) const DATA_DIR_ENV: &str = "FLOPTLE_DATA_DIR";

/// The folder name a build uses: the manifest's `data_id` when it is a valid
/// one, else one made from the title (a build exported before `data_id`
/// existed, or a hand-edited manifest).
pub(crate) fn data_id_for(data_id: Option<&str>, title: &str) -> String {
    match data_id {
        Some(id) if floptle_scene::valid_data_id(id) => id.to_string(),
        _ => floptle_scene::data_id_from_title(title),
    }
}

/// The folder under a platform's data directory. The studio folder is a
/// Windows convention only; macOS and Linux put an app's folder straight
/// under theirs.
fn layout(base: &Path, windows: bool, studio: Option<&str>, data_id: &str) -> PathBuf {
    let mut p = base.to_path_buf();
    if windows && let Some(s) = studio.filter(|s| floptle_scene::valid_data_id(s)) {
        p.push(s);
    }
    p.push(data_id);
    p
}

/// This user's data folder for the game `data_id`, not yet created.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn game_data_dir(studio: Option<&str>, data_id: &str) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os(DATA_DIR_ENV).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let base = directories::BaseDirs::new()?;
    Some(layout(base.data_dir(), cfg!(windows), studio, data_id))
}

/// Whether `dir` holds at least one file anywhere under it.
fn has_files(dir: &Path) -> bool {
    let Ok(entries) = floptle_vfs::read_dir(dir) else { return false };
    entries.iter().any(|e| if e.is_dir() { has_files(&e.path()) } else { floptle_vfs::is_file(e.path()) })
}

/// Copy every file under `from` into `to`, keeping the layout. Links are not
/// followed. Returns how many files were copied.
fn copy_files(from: &Path, to: &Path) -> std::io::Result<usize> {
    floptle_vfs::create_dir_all(to)?;
    let mut n = 0;
    for e in floptle_vfs::read_dir(from)? {
        let src = e.path();
        let Some(name) = src.file_name() else { continue };
        if floptle_vfs::is_symlink(&src) {
            continue;
        }
        if e.is_dir() {
            n += copy_files(&src, &to.join(name))?;
        } else if floptle_vfs::is_file(&src) {
            floptle_vfs::copy(&src, to.join(name))?;
            n += 1;
        }
    }
    Ok(n)
}

/// Bring a build's own `save/` and `replays/` into the data folder, once.
///
/// Every build made before the data folder existed kept these beside the
/// game. When the data folder has none of its own yet, what the build folder
/// holds is copied over, so a player updating in place keeps their progress.
/// Copied, not moved: the install folder may be read-only, and a player who
/// goes back to the old build still finds their files there. Returns one line
/// per folder copied, for the log.
pub(crate) fn adopt_build_data(build_project: &Path, data: &Path) -> Vec<String> {
    let mut notes = Vec::new();
    for dir in crate::export::RUNTIME_DIRS {
        let src = build_project.join(dir);
        let dst = data.join(dir);
        if !has_files(&src) || has_files(&dst) {
            continue;
        }
        match copy_files(&src, &dst) {
            Ok(n) => notes.push(format!(
                "copied {n} file{} from this build's {dir}/ into {} — the player's data lives \
                 there now",
                if n == 1 { "" } else { "s" },
                dst.display()
            )),
            Err(e) => notes.push(format!(
                "could not copy this build's {dir}/ into {}: {e} — the old files are still in {}",
                dst.display(),
                src.display()
            )),
        }
    }
    notes
}

/// Open the data folder for an exported build: work it out, create it, and
/// adopt the build's own saves if it is new. `None` (with a line saying why)
/// when this machine has no such folder or it cannot be made; the game then
/// keeps its data beside itself, as builds always did.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn open_for_build(
    data_id: Option<&str>,
    studio: Option<&str>,
    title: &str,
    build_project: &Path,
) -> Option<PathBuf> {
    let id = data_id_for(data_id, title);
    let Some(dir) = game_data_dir(studio, &id) else {
        floptle_say::say_err!(
            "this system has no per-user data folder to keep the game's saves in; they stay \
             beside the game in {}",
            build_project.display()
        );
        return None;
    };
    if let Err(e) = floptle_vfs::create_dir_all(&dir) {
        floptle_say::say_err!(
            "could not create the game's data folder {} ({e}); saves stay beside the game in {}",
            dir.display(),
            build_project.display()
        );
        return None;
    }
    for note in adopt_build_data(build_project, &dir) {
        floptle_say::say!("{note}");
    }
    Some(dir)
}

impl crate::Editor {
    /// Hand the script host both roots: the project for the game's own files,
    /// the data folder (if any) for the player's.
    pub(crate) fn point_scripts_at_project(&self) {
        self.script_host.set_project_root(self.project_root.clone());
        self.script_host.set_data_root(self.data_root.clone());
    }

    /// The folder that holds `save/` and `replays/`: the data folder in an
    /// exported build, the project everywhere else.
    pub(crate) fn runtime_base(&self) -> &Path {
        self.data_root.as_deref().unwrap_or(&self.project_root)
    }

    /// The folder `user://` names: the player's own files.
    pub(crate) fn user_dir(&self) -> PathBuf {
        floptle_script::paths::user_dir_for(&self.project_root, self.data_root.as_deref())
    }

    /// Where the open scene's sidecars live — its map geometry, paint and
    /// terrain fields. A scene from the player's files (`user://`) keeps them
    /// under `user://`, laid out the way a project lays them out
    /// (`user://maps/<scene>.map.ron`): a downloaded level must not pick up the
    /// game's own `maps/<scene>` just because the names match.
    pub(crate) fn sidecar_root(&self) -> PathBuf {
        if self.scene_rel.starts_with(floptle_script::paths::USER_PREFIX) {
            self.user_dir()
        } else {
            self.project_root.clone()
        }
    }

    /// [`Self::sidecar_root`] for a scene file that is not the open one.
    pub(crate) fn sidecar_root_of(&self, scene: &Path) -> PathBuf {
        let user = self.user_dir();
        if scene.starts_with(&user) { user } else { self.project_root.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_studio_folder_is_windows_only() {
        let base = Path::new("/base");
        assert_eq!(layout(base, true, Some("Fopull"), "freeflier"), Path::new("/base/Fopull/freeflier"));
        assert_eq!(layout(base, true, None, "freeflier"), Path::new("/base/freeflier"));
        assert_eq!(layout(base, false, Some("Fopull"), "freeflier"), Path::new("/base/freeflier"));
        // A studio that could leave the folder is not used.
        assert_eq!(layout(base, true, Some(".."), "freeflier"), Path::new("/base/freeflier"));
    }

    #[test]
    fn a_manifest_without_a_valid_id_falls_back_to_the_title() {
        assert_eq!(data_id_for(Some("freeflier"), "Free Flier"), "freeflier");
        assert_eq!(data_id_for(None, "Free Flier!"), "free-flier");
        assert_eq!(data_id_for(Some("../x"), "Free Flier"), "free-flier");
    }

    #[test]
    fn a_builds_saves_are_adopted_once_and_never_over_newer_data() {
        let tmp = std::env::temp_dir().join(format!("floptle-adopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let build = tmp.join("build/assets");
        let data = tmp.join("data");
        std::fs::create_dir_all(build.join("save")).unwrap();
        std::fs::create_dir_all(build.join("replays/old")).unwrap();
        std::fs::write(build.join("save/default.ron"), "{\"best\": 12}").unwrap();
        std::fs::write(build.join("replays/old/match.floptlereplay"), [1u8, 2, 3]).unwrap();

        let notes = adopt_build_data(&build, &data);
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert_eq!(std::fs::read_to_string(data.join("save/default.ron")).unwrap(), "{\"best\": 12}");
        assert_eq!(std::fs::read(data.join("replays/old/match.floptlereplay")).unwrap(), [1, 2, 3]);
        // The build's copy is left where it was.
        assert!(build.join("save/default.ron").is_file());

        // The player plays on: the data folder is now the newer truth, and a
        // second launch must not copy the build's older save over it.
        std::fs::write(data.join("save/default.ron"), "{\"best\": 9}").unwrap();
        assert!(adopt_build_data(&build, &data).is_empty());
        assert_eq!(std::fs::read_to_string(data.join("save/default.ron")).unwrap(), "{\"best\": 9}");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
