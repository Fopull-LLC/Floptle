//! The **dedicated server** binary.
//!
//! A shim over `floptle_editor::dedicated` — the same server `floptle serve`
//! runs, which is the engine's own play/host tick with no window and no local
//! player. See `crates/floptle-editor/src/dedicated.rs` for why there is only
//! one of those.
//!
//! ```text
//! floptle-server <project|--build DIR> [--scene scenes/x.ron]
//!                [--port 7777 | --relay host:port] [--tick 60]
//!                [--interest 150] [--budget 16384]
//!                [--max-players N] [--status-file PATH] [--game-key KEY]
//! ```
//!
//! **What makes this a different binary from `floptle-player` rather than a
//! flag on it**: it is built without the `devices` feature, so it links neither
//! `libasound.so.2` nor `libudev.so.1`. Those are load-time dependencies, not
//! runtime lookups, so a player build does not merely find no sound card on a
//! minimal server image — it fails to start at all, before `main`, with a
//! loader error about audio on a machine nobody is listening to.

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match floptle_editor::dedicated::ServerArgs::parse_argv(&argv) {
        Ok(args) => std::process::exit(floptle_editor::dedicated::run(args)),
        Err(e) => {
            eprintln!("floptle-server: {e}");
            std::process::exit(2);
        }
    }
}
