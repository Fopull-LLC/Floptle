//! The runtime's library face — currently just the dedicated server.
//!
//! `floptle-runtime` is a binary, and the server inside it is the only piece
//! anything else needs to call: `floptle serve` is the same server, reached
//! from the editor's command line so there is one place to type things.
//!
//! It is exposed as a library rather than copied for the reason the whole CLI
//! follows — a second implementation drifts and one call cannot. `server.rs`
//! already referenced nothing else in this crate, so this costs a file.

pub mod server;
