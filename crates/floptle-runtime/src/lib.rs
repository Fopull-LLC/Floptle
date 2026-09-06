//! The runtime's library face — currently just the dedicated server, and that
//! is now somebody else's code.
//!
//! `floptle-runtime --server` and `floptle serve` are the same server, and as
//! of this release so is the *engine* under it: `floptle_editor::dedicated` is
//! the editor's own play/host tick with no window and no local player.
//!
//! **This crate used to carry a second, smaller server**, re-derived from that
//! tick once and never caught up with it. It drained no `NetCmd` at all — so
//! `net.spawn`, `net.despawn`, `net.setOwner`, `net.kick`, `net.setRelevant`
//! and a server-originated `net.send` were silent no-ops on a dedicated server
//! — had no lag-compensation history, passed no terrain volumes, hard-coded
//! uniform gravity, never loaded a project's packages, and never stepped
//! animation or nav. None of that was a bug anyone wrote; it was the cost of
//! there being two of something. So there is one, and this is the alias that
//! keeps the flag working.

// **The dedicated server needs to LISTEN**, on QUIC or through a relay, and a
// browser tab cannot: it can open connections, never accept them. There is also
// nothing a browser build would do with it — a web export is a client. Same
// gate the transport itself carries in `floptle-net`.
#[cfg(not(target_arch = "wasm32"))]
pub use floptle_editor::dedicated as server;
