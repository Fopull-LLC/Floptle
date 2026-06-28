//! # Floptle Runtime
//!
//! The headless-of-editor game player. An exported game is this runtime plus a
//! packed project. Also the basis for a future dedicated `server` build.
//!
//! **Phase 1 skeleton.** The engine's core loop (clock + deterministic fixed-step
//! + ECS systems) is live and exercised headlessly below. The next fill-in is
//! windowing + GPU: open a winit window, build `floptle_render::Gpu`, and drive
//! [`app::App::frame`] from `redraw` with a real raster pass (ROADMAP Phase 1).

mod app;
use app::App;

fn main() {
    println!(
        "{} runtime v{} — Phase 1 skeleton",
        floptle_core::ENGINE_NAME,
        floptle_core::ENGINE_VERSION
    );

    // Headless: drive the real core loop for ~2 simulated seconds at 60 fps (no
    // GPU yet) to prove the clock + fixed-step + ECS systems work end to end.
    let mut app = App::new();
    let dt = 1.0 / 60.0;
    for _ in 0..120 {
        app.frame(dt);
    }

    println!(
        "  core loop ok — {} frames, {:.2}s sim time, {} live entit{}",
        app.time.frame,
        app.time.elapsed,
        app.world.len(),
        if app.world.len() == 1 { "y" } else { "ies" }
    );
    println!(
        "  next: open a window, create floptle_render::Gpu, render App::frame (ROADMAP Phase 1)."
    );
}
