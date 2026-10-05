//! Absolute paths in a project's files: see [`floptle_scene::portable`], where
//! the finding and rewriting live so every crate that writes an authored file
//! shares them.

pub(crate) use floptle_scene::portable::{find, relativize};
