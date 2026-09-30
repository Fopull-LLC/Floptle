//! Where a script's files live: the game's own, and the player's.
//!
//! A path like `"levels/intro.json"` is the game's: relative to the project,
//! shipped with the build, and read-only once it is installed. A path like
//! `"user://levels/mine.json"` is the player's: settings, progress,
//! player-made levels, replays, screenshots. It lives outside the build, so
//! it survives installing a new version.
//!
//! | Host | save slots | `user://` |
//! |---|---|---|
//! | editor, `floptle run` / `shot` / `play` | `<project>/save/` | `<project>/save/user/` |
//! | an exported build | `<data>/save/` | `<data>/user/` |
//! | a browser build | the page's storage, as `save/` | as `save/user/` |
//!
//! `<data>` is the per-user folder the driver works out for an exported game
//! (see `floptle_scene::user_data_dir`). In development both stay inside
//! `save/`, which an export already leaves out, so a test run never touches a
//! player's real files and a build never ships the developer's.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// The prefix that names the player's own files.
pub const USER_PREFIX: &str = "user://";

/// The folder `user://` points at.
pub fn user_dir_for(project: &Path, data: Option<&Path>) -> PathBuf {
    match data {
        Some(d) => d.join("user"),
        None => project.join("save").join("user"),
    }
}

/// The folder that holds `save/` (slots, crash reports) and `replays/`.
pub fn runtime_base_for(project: &Path, data: Option<&Path>) -> PathBuf {
    data.unwrap_or(project).to_path_buf()
}

/// The two roots a script's paths resolve against, shared by `assets.*`,
/// `save.*` and `app.dataPath`.
#[derive(Clone)]
pub(crate) struct Paths {
    pub(crate) project: Rc<RefCell<PathBuf>>,
    /// `Some` in an exported build: the per-user data folder.
    pub(crate) data: Rc<RefCell<Option<PathBuf>>>,
}

impl Paths {
    pub(crate) fn new() -> Self {
        Self::at(PathBuf::from("assets"))
    }

    pub(crate) fn at(project: PathBuf) -> Self {
        Self { project: Rc::new(RefCell::new(project)), data: Rc::default() }
    }

    pub(crate) fn user_dir(&self) -> PathBuf {
        user_dir_for(&self.project.borrow(), self.data.borrow().as_deref())
    }

    pub(crate) fn runtime_base(&self) -> PathBuf {
        runtime_base_for(&self.project.borrow(), self.data.borrow().as_deref())
    }

    /// The folder a script's `path` is relative to, the rest of the path, and
    /// whether it named the player's files.
    pub(crate) fn split<'a>(&self, path: &'a str) -> (PathBuf, &'a str, bool) {
        match path.strip_prefix(USER_PREFIX) {
            Some(rest) => (self.user_dir(), rest, true),
            None => (self.project.borrow().clone(), path, false),
        }
    }
}
