//! Floptle Hub — the cross-platform launcher / version manager / project manager.
//! See ADR-0021 and docs/updating-the-hub.md.
//!
//! Deliberately light: it installs & launches engine versions and tracks projects, and
//! depends only on `floptle-scene` (to read a project's `project.ron`) — never on the
//! render/editor crates. Engine versions are self-contained bundles unpacked under a
//! per-user data dir; the editor is launched as a child process for a chosen project.

mod app;
mod config;
mod install;
mod launch;
mod notes;
mod registry;
mod releases;
mod selfupdate;

use app::HubApp;
use config::Paths;

/// The device flow moved to `floptle-account` when the engine needed it too — a game
/// exported from Floptle runs on machines that never installed the Hub. Re-exported under
/// its old path so every `crate::auth::…` here still reads the same.
pub use floptle_account::auth;

fn main() -> eframe::Result<()> {
    env_logger::init();

    // Sweep the binary a previous self-update moved aside. Startup is the one moment
    // nothing has it open — on Windows that is the *only* moment it can be deleted.
    selfupdate::clean_leftovers();

    let paths = Paths::resolve().unwrap_or_else(|| {
        // No home dir (unusual) — fall back to a `.floptle-hub` next to the cwd.
        eprintln!("could not resolve a home directory; using ./.floptle-hub");
        Paths::at(std::path::Path::new("./.floptle-hub"))
    });

    // The desktop learns what this window is BEFORE it opens (Linux): a
    // Wayland compositor shows the icon of the `.desktop` entry whose name is
    // the window's app_id, and nothing else — a window icon handed to it is
    // ignored. The Hub is the thing that is installed, so it writes the
    // entries for itself and for the editor it launches, every start,
    // rewriting nothing that is already right.
    #[cfg(target_os = "linux")]
    if let (Some(home), Ok(me)) = (floptle_brand::linux::data_home(), std::env::current_exe()) {
        let editor = registry::scan_installs(&paths.versions_dir())
            .into_iter()
            .rev()
            .find(|i| i.is_valid())
            .map(|i| i.editor_bin());
        if let Err(e) = floptle_brand::linux::install(&home, &me, editor.as_deref(), true) {
            eprintln!("could not write the desktop entries: {e}");
        }
    }

    let icon = floptle_brand::Icon::at(256).expect("the committed icon decodes");
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("Floptle Hub")
            .with_app_id(floptle_brand::HUB_APP_ID)
            .with_icon(eframe::egui::IconData { rgba: icon.rgba, width: icon.width, height: icon.height })
            .with_inner_size([880.0, 620.0])
            .with_min_inner_size([560.0, 400.0]),
        ..Default::default()
    };

    eframe::run_native("Floptle Hub", options, Box::new(move |_cc| Ok(Box::new(HubApp::new(paths)))))
}
