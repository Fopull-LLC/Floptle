//! # Floptle Runtime
//!
//! The headless-of-editor game player. An exported game is this runtime plus a
//! packed project. Also the basis for a future dedicated `server` build.
//!
//! **Phase 1.** Opens a window, creates the GPU, and drives the real core loop
//! (clock + deterministic fixed-step + ECS systems) on every redraw — currently
//! rendering a clear pass with FPS in the title. Pass `--headless` to run the loop
//! without a window (CI / no-display): it advances ~2 simulated seconds and exits.

mod app;
mod camera;
mod runner;

use app::App;

fn main() {
    env_logger::init();
    println!("{} runtime v{}", floptle_core::ENGINE_NAME, floptle_core::ENGINE_VERSION);

    if std::env::args().any(|a| a == "--headless") {
        headless_selfcheck();
    } else {
        runner::run();
    }
}

/// Drive the real core loop for ~2 simulated seconds at 60 fps with no GPU, so the
/// engine is verifiable where there's no display.
fn headless_selfcheck() {
    let mut app = App::new();
    let dt = 1.0 / 60.0;
    for _ in 0..120 {
        app.frame(dt);
    }
    println!(
        "  headless core loop ok — {} frames, {:.2}s sim time, {} live entit{}",
        app.time.frame,
        app.time.elapsed,
        app.world.len(),
        if app.world.len() == 1 { "y" } else { "ies" }
    );
}
