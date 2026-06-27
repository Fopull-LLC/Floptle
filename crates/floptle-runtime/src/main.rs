//! # Floptle Runtime
//!
//! The headless-of-editor game player. An exported game is this runtime plus a
//! packed project. Also the basis for a future dedicated `server` build
//! (ADR/networking is deferred). This is a planning stub.

fn main() {
    println!(
        "{} runtime v{} — planning scaffold.",
        floptle_core::ENGINE_NAME,
        floptle_core::ENGINE_VERSION
    );
}
