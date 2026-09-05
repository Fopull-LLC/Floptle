// Release builds on Windows are GUI apps (no console window behind the game —
// exports ship this binary); debug keeps the console for logs.
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]
//! The editor binary (`floptle`).
//!
//! A shim: everything is in the library half of this crate, so the headless
//! verbs, the editor window and the standalone player can all drive the same
//! code instead of three copies of it. See `lib.rs`.

fn main() {
    floptle_editor::run();
}
