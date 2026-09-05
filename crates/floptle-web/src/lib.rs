//! The browser build of the engine: one wasm module, two entry points.
//!
//! **`play(bundle)`** is the game. The page (`web/index.html`, which an export
//! ships beside the game's bundle) fetches `game.flpk`, and this mounts it as
//! the engine's filesystem and starts the same player the desktop runs — see
//! `floptle_editor::player::web`. Everything the game does from there is the
//! engine library with the editor half compiled out, on WebGPU.
//!
//! **`probe()`** is the bring-up ladder from docs/web-export.md step 4: the
//! scripting VM in a tab, a device, every shader through the browser's own
//! compiler, and a skinned, vertex-painted mesh through the real raster pass.
//! Each rung is a line on its page (`web/probe.html`), so a headless browser
//! can say which one failed. It stays because it is the fastest answer to
//! "did a shader change break the browser" — `tools/web/shot.py`.
//!
//! Nothing in here is reachable from the desktop. `bar` is the probe's rig,
//! kept target-independent so its arithmetic has ordinary tests.
//!
//! Build: `tools/web/build.sh` (needs the WASI SDK — `tools/web/env.sh`
//! fetches it). Run: `tools/web/shot.py`, or serve `target/web/` and open it.

pub mod bar;

#[cfg(target_arch = "wasm32")]
mod wasi;

#[cfg(target_arch = "wasm32")]
mod probe;

#[cfg(target_arch = "wasm32")]
mod player;
