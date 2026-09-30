// A shipped game is a GUI app on Windows: no console window behind it.
#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]
//! The standalone player with Steam compiled in — the same shim as `main.rs`,
//! built as its own binary so the plain player never links Steam's library.

fn main() {
    floptle_editor::run_player();
}
