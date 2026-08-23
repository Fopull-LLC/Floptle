//! # floptle-steam — the Steamworks-backed `floptle_services::Platform` impl
//!
//! Phase 0 of `docs/steam-integration-proposal.md`. Everything real here sits
//! behind the `steam` cargo feature (off by default): with it off, this crate
//! is deliberately near-empty — there is nothing to implement yet, because no
//! phase has landed a capability's methods on `floptle_services`' sub-traits,
//! and the `steamworks` dependency itself needs the Steamworks SDK present at
//! build time, which most builds of this workspace (CI's default gate
//! included) never provide.
//!
//! What lands here, phase by phase: [`SteamPlatform`]'s init/pump/shutdown
//! lifecycle and identity (Phase 1), `impl Transport for SteamTransport`
//! (Phase 6), and the rest of `floptle_services`' sub-traits as each phase
//! gives them methods to implement.

#![warn(missing_docs)]

// Proves the optional `steamworks` dependency actually resolves and links
// under this feature, ahead of any phase that calls into it.
#[cfg(feature = "steam")]
use steamworks as _;

#[cfg(feature = "steam")]
use floptle_services::Platform;

/// The Steamworks-backed platform backend. Empty today — every
/// [`Platform`] accessor answers `None`, identically to
/// [`NullPlatform`](floptle_services::NullPlatform), because no phase has
/// given a capability anything to construct yet. Phase 1 adds the actual
/// `SteamAPI_Init`/pump/shutdown lifecycle and starts populating `identity()`.
#[cfg(feature = "steam")]
#[derive(Debug, Default, Clone, Copy)]
pub struct SteamPlatform;

#[cfg(feature = "steam")]
impl Platform for SteamPlatform {}

#[cfg(all(test, feature = "steam"))]
mod tests {
    use super::*;

    #[test]
    fn steam_platform_is_a_platform() {
        let p = SteamPlatform;
        let dyn_p: &dyn Platform = &p;
        assert!(dyn_p.identity().is_none());
    }
}
