//! Background work, on platforms that may not have a background.
//!
//! The engine hands long jobs — a navmesh bake, a terrain remesh, a planet
//! being generated — to a thread and drains the answer through a channel a
//! frame or many later. `wasm32-unknown-unknown` has no threads: `spawn`
//! compiles there and panics. So every such job goes through [`spawn`], which
//! is a thread on the desktop and, in a browser, runs the job right here and
//! now. The channel still carries the answer, so the code that drains it is
//! the same on both; what changes is that a browser build pays for the job on
//! the frame that asked for it. That is the honest v1: a stall you can see,
//! rather than a worker pool that needs cross-origin isolation headers on
//! every host the game is served from.

/// Run `f` off the main thread where there is one, inline where there is not.
///
/// `name` names the thread (it shows in a profiler and a panic message);
/// unused in a browser.
pub(crate) fn spawn<F>(name: &str, f: F)
where
    F: FnOnce() + Send + 'static,
{
    #[cfg(not(target_arch = "wasm32"))]
    {
        if let Err(e) = std::thread::Builder::new().name(name.to_string()).spawn(f) {
            // A thread that cannot start is a machine at its limits; the job
            // is lost and the drain sees a closed channel, which every caller
            // already treats as "the answer is not coming".
            log::warn!("could not start the {name} worker: {e}");
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = name;
        f();
    }
}
