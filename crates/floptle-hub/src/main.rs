//! Floptle Hub — the cross-platform launcher / version manager / project manager.
//! See ADR-0021 and docs/hub-proposal.md.
//!
//! Deliberately light: it installs & launches engine versions and tracks projects, and
//! depends only on `floptle-scene` (to read a project's `project.ron`) — never on the
//! render/editor crates. Engine versions are self-contained bundles unpacked under a
//! per-user data dir; the editor is launched as a child process for a chosen project.

mod app;
mod auth;
mod config;
mod install;
mod launch;
mod registry;
mod releases;

use app::HubApp;
use config::Paths;

fn main() -> eframe::Result<()> {
    env_logger::init();

    let paths = Paths::resolve().unwrap_or_else(|| {
        // No home dir (unusual) — fall back to a `.floptle-hub` next to the cwd.
        eprintln!("could not resolve a home directory; using ./.floptle-hub");
        Paths::at(std::path::Path::new("./.floptle-hub"))
    });

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Floptle Hub")
            .with_inner_size([880.0, 620.0])
            .with_min_inner_size([560.0, 400.0]),
        ..Default::default()
    };

    eframe::run_native("Floptle Hub", options, Box::new(move |_cc| Ok(Box::new(HubApp::new(paths)))))
}
