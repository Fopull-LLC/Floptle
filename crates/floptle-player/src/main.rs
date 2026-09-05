// A shipped game is a GUI app on Windows: no console window behind it.
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]
//! The standalone player binary.
//!
//! A shim over `floptle_editor::run_player` — see that module for why the
//! engine and the editor share a crate, and how the editor half is compiled
//! out from under this build.

fn main() {
    floptle_editor::run_player();
}
