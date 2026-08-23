//! # floptle-steam — the Steamworks-backed `floptle_services::Platform` impl
//!
//! Everything real here sits behind the `steam` cargo feature (off by
//! default): with it off, this crate is empty, because the `steamworks`
//! dependency itself needs the Steamworks SDK present at build time, which
//! most builds of this workspace (CI's default gate included) never provide.
//!
//! Phase 1 of `docs/steam-integration-proposal.md`: [`SteamPlatform`]'s
//! init/pump/shutdown lifecycle and [`floptle_services::Identity`] (local
//! user + app/build info). Phase 6 adds `impl Transport for SteamTransport`;
//! the rest of `floptle_services`' sub-traits land as their own phases give
//! them methods to implement.
//!
//! **Callers, not this crate, own `restart_app_if_necessary` and any
//! logging.** [`restart_app_if_necessary`] is re-exported so a caller can run
//! it BEFORE engine startup and exit immediately if it returns `true` — never
//! call [`SteamPlatform::init`] first. `init` returns a plain `Result`; a
//! caller unable to init (no Steam client running, most commonly) decides for
//! itself whether that's a warning worth surfacing and falls back to
//! [`floptle_services::NullPlatform`].

#![warn(missing_docs)]

#[cfg(feature = "steam")]
use std::sync::Arc;
#[cfg(feature = "steam")]
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(feature = "steam")]
use floptle_services::{Identity, Platform};

/// Returns `true` if the app wasn't launched through Steam and Steam has
/// begun relaunching it — the caller should exit as soon as possible and
/// must NOT go on to call [`SteamPlatform::init`]. Re-exports
/// `steamworks::restart_app_if_necessary`.
#[cfg(feature = "steam")]
pub fn restart_app_if_necessary(app_id: u32) -> bool {
    steamworks::restart_app_if_necessary(app_id.into())
}

/// The Steamworks-backed platform backend: a live `SteamAPI_Init`'d client.
/// Every [`Platform`] accessor beyond [`Identity`] still answers `None` —
/// nothing has given Achievements/Cloud/Overlay/etc. anything to implement
/// yet.
#[cfg(feature = "steam")]
pub struct SteamPlatform {
    client: steamworks::Client,
    app_id: steamworks::AppId,
    persona_changed: Arc<AtomicBool>,
    // Held only to keep the registration alive — dropping it unregisters the
    // callback. Never read directly.
    _persona_cb: steamworks::CallbackHandle,
}

#[cfg(feature = "steam")]
impl SteamPlatform {
    /// Initializes the Steamworks API for `app_id` and registers the
    /// persona-state-change callback [`Identity::poll_persona_change`]
    /// drains. Call [`restart_app_if_necessary`] first and exit if it
    /// returns `true` — this must never run in that case.
    ///
    /// Fails if no Steam client is running, the app isn't set up on the
    /// Steamworks backend, or the user doesn't own a license — all real,
    /// expected outcomes in dev (see the Spacewar-480 fallback in
    /// `docs/steam-integration-proposal.md`), not bugs.
    pub fn init(app_id: u32) -> Result<Self, steamworks::SteamAPIInitError> {
        let client = steamworks::Client::init_app(app_id)?;
        let persona_changed = Arc::new(AtomicBool::new(false));
        let flag = persona_changed.clone();
        let _persona_cb = client.register_callback::<steamworks::PersonaStateChange, _>(move |_| {
            flag.store(true, Ordering::Relaxed);
        });
        Ok(Self { client, app_id: app_id.into(), persona_changed, _persona_cb })
    }
}

#[cfg(feature = "steam")]
impl Platform for SteamPlatform {
    fn available(&self) -> bool {
        true
    }
    fn pump(&self) {
        self.client.run_callbacks();
    }
    fn identity(&self) -> Option<&dyn Identity> {
        Some(self)
    }
}

#[cfg(feature = "steam")]
impl Identity for SteamPlatform {
    fn local_user_id(&self) -> u64 {
        self.client.user().steam_id().raw()
    }
    fn persona_name(&self) -> String {
        self.client.friends().name()
    }
    fn avatar_small(&self) -> Option<Vec<u8>> {
        self.client.friends().get_friend(self.client.user().steam_id()).small_avatar()
    }
    fn avatar_medium(&self) -> Option<Vec<u8>> {
        self.client.friends().get_friend(self.client.user().steam_id()).medium_avatar()
    }
    fn avatar_large(&self) -> Option<Vec<u8>> {
        self.client.friends().get_friend(self.client.user().steam_id()).large_avatar()
    }
    fn poll_persona_change(&self) -> bool {
        self.persona_changed.swap(false, Ordering::Relaxed)
    }
    fn build_id(&self) -> i32 {
        self.client.apps().app_build_id()
    }
    fn install_dir(&self) -> String {
        self.client.apps().app_install_dir(self.app_id)
    }
    fn beta_name(&self) -> Option<String> {
        self.client.apps().current_beta_name()
    }
    fn is_family_shared(&self) -> bool {
        self.client.apps().app_owner() != self.client.user().steam_id()
    }
    fn is_cybercafe(&self) -> bool {
        self.client.apps().is_cybercafe()
    }
}

#[cfg(all(test, feature = "steam"))]
mod tests {
    use super::*;

    /// `SteamPlatform` never actually inits without a live Steam client
    /// (Spacewar 480, no client running in CI) — this exercises the one path
    /// that IS reachable headless: the failure itself is a plain `Result`,
    /// not a panic, which is what lets a caller fall back to `NullPlatform`
    /// rather than crash.
    #[test]
    fn init_without_a_steam_client_fails_cleanly_not_a_panic() {
        let result = SteamPlatform::init(480);
        assert!(result.is_err(), "no Steam client is running in this test environment");
    }

    #[test]
    fn restart_app_if_necessary_does_not_panic() {
        // With no `steam_appid.txt` and no Steam client, this just answers
        // false — asserting it runs at all is the point (it must be safe to
        // call unconditionally, before any other Steam call).
        let _ = restart_app_if_necessary(480);
    }
}
