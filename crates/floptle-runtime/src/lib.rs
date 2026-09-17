//! The runtime's library face — the dedicated server, which is the editor's
//! code.
//!
//! `floptle-runtime --server` and `floptle serve` are the same server, and as
//! of this release so is the *engine* under it: `floptle_editor::dedicated` is
//! the editor's own play/host tick with no window and no local player.
//!
//! One server, not a second smaller one re-derived from that tick: a copy
//! drifts — no `NetCmd` drained, so `net.spawn`, `net.kick` and the rest go
//! silent on a dedicated server; no lag-compensation history; no terrain
//! volumes; uniform gravity; no packages; no animation or nav. This is the
//! alias that keeps the flag working.

// **The dedicated server needs to LISTEN**, on QUIC or through a relay, and a
// browser tab cannot: it can open connections, never accept them. There is also
// nothing a browser build would do with it — a web export is a client. Same
// gate the transport itself carries in `floptle-net`.
#[cfg(not(target_arch = "wasm32"))]
pub use floptle_editor::dedicated as server;
