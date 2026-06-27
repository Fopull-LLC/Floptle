//! # floptle-net
//!
//! Deferred (NOT a launch requirement). Exists now only to hold the boundary so
//! gameplay code stays "transport-agnostic" and we can add an authoritative
//! server build later without reshaping the engine. You self-host already, so
//! the target model is: a dedicated `server` build of a game + connecting
//! clients. See `docs/subsystems/networking-future.md`.
//!
//! Planned (future) modules:
//! - `role`      : Client vs Server vs ListenServer.
//! - `replicate` : which node/component state syncs, and how.
//! - `transport` : UDP/QUIC transport (renet / quinn) behind a trait.
