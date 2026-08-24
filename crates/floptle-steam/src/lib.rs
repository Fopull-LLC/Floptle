//! # floptle-steam — the Steamworks-backed `floptle_services::Platform` impl
//!
//! Everything real here sits behind the `steam` cargo feature (off by
//! default): with it off, this crate is empty, because the `steamworks`
//! dependency itself needs the Steamworks SDK present at build time, which
//! most builds of this workspace (CI's default gate included) never provide.
//!
//! [`SteamPlatform`]'s init/pump/shutdown lifecycle landed Phase 1, alongside
//! [`floptle_services::Identity`] (local user + app/build info, later
//! extended with environment facts in Phase 13). [`floptle_services::
//! Achievements`] (achievements + stats) landed Phase 2,
//! [`floptle_services::Cloud`] (cloud saves) Phase 4. Phase 6 adds `impl
//! Transport for SteamTransport`; the rest of `floptle_services`' sub-traits
//! land as their own phases give them methods to implement.
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
use std::cell::Cell;
#[cfg(feature = "steam")]
use std::time::{Duration, Instant};

#[cfg(feature = "steam")]
use std::io::{Read, Write};

#[cfg(feature = "steam")]
use floptle_services::{Achievements, Cloud, FriendInfo, Identity, Platform, Social};

/// How long to leave achievement/stat writes queued before an automatic
/// [`Achievements::flush`] — a game that never calls `flush` itself still
/// reaches the server, just not on every single write.
#[cfg(feature = "steam")]
const AUTO_FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// Whether enough time has passed since the last flush attempt to try again —
/// pulled out pure so the boundary (never on the very first pump, never
/// before the interval elapses, always once it has) is testable without a
/// live Steam session.
#[cfg(feature = "steam")]
fn flush_is_due(last_attempt: Option<Instant>, now: Instant, interval: Duration) -> bool {
    match last_attempt {
        None => true,
        Some(t) => now.saturating_duration_since(t) >= interval,
    }
}

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
    /// Set by the `UserStatsReceived` callback — every `Achievements` method
    /// answers honestly (`None`/`Err`) before this, rather than guessing.
    /// This SDK version has no `RequestCurrentStats` to wait on explicitly
    /// (removed from the interface Valve ships in this `steamworks-sys`
    /// vendor — confirmed against the header, not guessed); stats arrive on
    /// their own shortly after init, and this flag is how a caller actually
    /// knows "shortly" has passed.
    stats_ready: Arc<AtomicBool>,
    /// Set on any local achievement/stat write, cleared once a `store_stats`
    /// call is accepted — but see `store_failed` for why "accepted" isn't
    /// "confirmed".
    dirty: Arc<AtomicBool>,
    /// Set by the `UserStatsStored` callback when a store's ASYNC server
    /// round-trip comes back failed (offline, a transient backend error) —
    /// `flush` re-checks this and re-marks `dirty` rather than trusting the
    /// synchronous `store_stats()` call's own `Ok` (which only means "the
    /// local request was accepted", not "the server confirmed it"). This is
    /// the whole of "offline queue + reconcile on reconnect": nothing is
    /// dropped, the next automatic batch or explicit `flush` just tries again.
    store_failed: Arc<AtomicBool>,
    last_flush_attempt: Cell<Option<Instant>>,
    // Held only to keep the registrations alive — dropping one unregisters
    // its callback. Never read directly.
    _persona_cb: steamworks::CallbackHandle,
    _stats_received_cb: steamworks::CallbackHandle,
    _stats_stored_cb: steamworks::CallbackHandle,
}

#[cfg(feature = "steam")]
impl SteamPlatform {
    /// Initializes the Steamworks API for `app_id` and registers the
    /// callbacks [`Identity::poll_persona_change`] and every [`Achievements`]
    /// method drain. Call [`restart_app_if_necessary`] first and exit if it
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

        let stats_ready = Arc::new(AtomicBool::new(false));
        let ready = stats_ready.clone();
        let _stats_received_cb =
            client.register_callback::<steamworks::UserStatsReceived, _>(move |cb| {
                if cb.result.is_ok() {
                    ready.store(true, Ordering::Relaxed);
                }
            });

        let store_failed = Arc::new(AtomicBool::new(false));
        let failed = store_failed.clone();
        let _stats_stored_cb = client.register_callback::<steamworks::UserStatsStored, _>(move |cb| {
            if cb.result.is_err() {
                failed.store(true, Ordering::Relaxed);
            }
        });

        Ok(Self {
            client,
            app_id: app_id.into(),
            persona_changed,
            stats_ready,
            dirty: Arc::new(AtomicBool::new(false)),
            store_failed,
            last_flush_attempt: Cell::new(None),
            _persona_cb,
            _stats_received_cb,
            _stats_stored_cb,
        })
    }
}

#[cfg(feature = "steam")]
impl Platform for SteamPlatform {
    fn available(&self) -> bool {
        true
    }
    fn pump(&self) {
        self.client.run_callbacks();
        // Reconcile FIRST: a store's async server round-trip can fail on a
        // frame long after `dirty` was already cleared for it, and the due/
        // dirty check right below is the only thing deciding whether `flush`
        // gets called at all — checking `store_failed` only INSIDE `flush`
        // would mean it's never read until something else re-dirties first.
        if self.store_failed.swap(false, Ordering::Relaxed) {
            self.dirty.store(true, Ordering::Relaxed);
        }
        let due = flush_is_due(self.last_flush_attempt.get(), Instant::now(), AUTO_FLUSH_INTERVAL);
        if due && self.dirty.load(Ordering::Relaxed) {
            Achievements::flush(self);
        }
    }
    fn identity(&self) -> Option<&dyn Identity> {
        Some(self)
    }
    fn achievements(&self) -> Option<&dyn Achievements> {
        Some(self)
    }
    fn cloud(&self) -> Option<&dyn Cloud> {
        Some(self)
    }
    fn social(&self) -> Option<&dyn Social> {
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
    fn ui_language(&self) -> String {
        self.client.utils().ui_language()
    }
    fn is_steam_deck(&self) -> bool {
        self.client.utils().is_steam_running_on_steam_deck()
    }
    fn is_big_picture_mode(&self) -> bool {
        self.client.utils().is_steam_in_big_picture_mode()
    }
}

/// `steamworks::FriendState` as the plain lowercase string
/// `floptle_services::FriendInfo::state` carries — see that field's doc for
/// why a string, not a typed enum, is the deliberate choice here.
#[cfg(feature = "steam")]
fn friend_state_str(state: steamworks::FriendState) -> String {
    match state {
        steamworks::FriendState::Offline => "offline",
        steamworks::FriendState::Online => "online",
        steamworks::FriendState::Invisible => "invisible",
        steamworks::FriendState::Busy => "busy",
        steamworks::FriendState::Away => "away",
        steamworks::FriendState::Snooze => "snooze",
        steamworks::FriendState::LookingToTrade => "looking to trade",
        steamworks::FriendState::LookingToPlay => "looking to play",
    }
    .to_string()
}

#[cfg(feature = "steam")]
impl Social for SteamPlatform {
    fn set_rich_presence(&self, key: &str, value: &str) -> Result<(), String> {
        if self.client.friends().set_rich_presence(key, Some(value)) {
            Ok(())
        } else {
            Err(format!(
                "Steam rejected rich-presence key \"{key}\" — too many keys set, or the key/value was too long"
            ))
        }
    }
    fn clear_rich_presence(&self) {
        self.client.friends().clear_rich_presence();
    }
    fn friends(&self) -> Vec<FriendInfo> {
        let my_app = self.app_id;
        self.client
            .friends()
            .get_friends(steamworks::FriendFlags::IMMEDIATE)
            .into_iter()
            .map(|f| FriendInfo {
                id: f.id().raw(),
                name: f.name(),
                state: friend_state_str(f.state()),
                playing_this_game: f.game_played().is_some_and(|g| g.game.app_id() == my_app),
            })
            .collect()
    }
    fn friend_rich_presence(&self, friend_id: u64, key: &str) -> Option<String> {
        self.client.friends().get_friend(steamworks::SteamId::from_raw(friend_id)).rich_presence(key)
    }
}

#[cfg(feature = "steam")]
impl Cloud for SteamPlatform {
    fn is_enabled_for_app(&self) -> bool {
        self.client.remote_storage().is_cloud_enabled_for_app()
    }
    fn set_enabled_for_app(&self, enabled: bool) {
        self.client.remote_storage().set_cloud_enabled_for_app(enabled);
    }
    fn is_enabled_for_account(&self) -> bool {
        self.client.remote_storage().is_cloud_enabled_for_account()
    }
    fn files(&self) -> Vec<(String, u64)> {
        self.client.remote_storage().files().into_iter().map(|f| (f.name, f.size)).collect()
    }
    fn file_exists(&self, name: &str) -> bool {
        self.client.remote_storage().file(name).exists()
    }
    fn file_timestamp(&self, name: &str) -> Option<i64> {
        let file = self.client.remote_storage().file(name);
        file.exists().then(|| file.timestamp())
    }
    fn delete_file(&self, name: &str) -> Result<(), String> {
        if self.client.remote_storage().file(name).delete() {
            Ok(())
        } else {
            Err(format!("\"{name}\" wasn't in Cloud storage to delete"))
        }
    }
    fn forget_file(&self, name: &str) -> Result<(), String> {
        if self.client.remote_storage().file(name).forget() {
            Ok(())
        } else {
            Err(format!("\"{name}\" wasn't in Cloud storage to forget"))
        }
    }
    fn read_file(&self, name: &str) -> Result<Vec<u8>, String> {
        let file = self.client.remote_storage().file(name);
        if !file.exists() {
            return Err(format!("\"{name}\" isn't in Cloud storage"));
        }
        let mut buf = Vec::new();
        file.read().read_to_end(&mut buf).map_err(|e| format!("reading \"{name}\": {e}"))?;
        Ok(buf)
    }
    fn write_file(&self, name: &str, data: &[u8]) -> Result<(), String> {
        let file = self.client.remote_storage().file(name);
        file.write().write_all(data).map_err(|e| format!("writing \"{name}\": {e}"))
    }
}

#[cfg(feature = "steam")]
impl Achievements for SteamPlatform {
    fn stats_ready(&self) -> bool {
        self.stats_ready.load(Ordering::Relaxed)
    }
    fn achievement_unlocked(&self, id: &str) -> Option<bool> {
        if !self.stats_ready() {
            return None;
        }
        self.client.user_stats().achievement(id).get().ok()
    }
    fn unlock_achievement(&self, id: &str) -> Result<(), String> {
        self.client
            .user_stats()
            .achievement(id)
            .set()
            .map_err(|()| self.achievement_write_failed(id))?;
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }
    fn clear_achievement(&self, id: &str) -> Result<(), String> {
        self.client
            .user_stats()
            .achievement(id)
            .clear()
            .map_err(|()| self.achievement_write_failed(id))?;
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }
    fn achievement_global_percent(&self, id: &str) -> Option<f32> {
        if !self.stats_ready() {
            return None;
        }
        self.client.user_stats().achievement(id).get_achievement_achieved_percent().ok()
    }
    fn achievement_name(&self, id: &str) -> Option<String> {
        if !self.stats_ready() {
            return None;
        }
        self.client
            .user_stats()
            .achievement(id)
            .get_achievement_display_attribute("name")
            .ok()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }
    fn achievement_description(&self, id: &str) -> Option<String> {
        if !self.stats_ready() {
            return None;
        }
        self.client
            .user_stats()
            .achievement(id)
            .get_achievement_display_attribute("desc")
            .ok()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }
    fn stat_int(&self, name: &str) -> Option<i32> {
        if !self.stats_ready() {
            return None;
        }
        self.client.user_stats().get_stat_i32(name).ok()
    }
    fn set_stat_int(&self, name: &str, value: i32) -> Result<(), String> {
        self.client
            .user_stats()
            .set_stat_i32(name, value)
            .map_err(|()| self.stat_write_failed(name))?;
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }
    fn stat_float(&self, name: &str) -> Option<f32> {
        if !self.stats_ready() {
            return None;
        }
        self.client.user_stats().get_stat_f32(name).ok()
    }
    fn set_stat_float(&self, name: &str, value: f32) -> Result<(), String> {
        self.client
            .user_stats()
            .set_stat_f32(name, value)
            .map_err(|()| self.stat_write_failed(name))?;
        self.dirty.store(true, Ordering::Relaxed);
        Ok(())
    }
    fn flush(&self) {
        // Callable directly (e.g. from Lua) without going through `pump`
        // first, so this reconciles too — see the comment in `pump`.
        if self.store_failed.swap(false, Ordering::Relaxed) {
            self.dirty.store(true, Ordering::Relaxed);
        }
        if !self.dirty.load(Ordering::Relaxed) {
            return;
        }
        self.last_flush_attempt.set(Some(Instant::now()));
        // The synchronous call only means "the local request was accepted" —
        // clear optimistically. If the actual async server round-trip later
        // comes back failed, the `UserStatsStored` callback sets
        // `store_failed`, and the reconcile above (on the NEXT `pump`/`flush`)
        // re-marks `dirty` so the write isn't lost. A synchronous `Err` here
        // (stats not ready yet, most likely) leaves `dirty` set so the next
        // attempt retries.
        if self.client.user_stats().store_stats().is_ok() {
            self.dirty.store(false, Ordering::Relaxed);
        }
    }
    fn reset_all_stats(&self, achievements_too: bool) -> Result<(), String> {
        self.client
            .user_stats()
            .reset_all_stats(achievements_too)
            .map_err(|()| self.not_ready_reason())
    }
}

#[cfg(feature = "steam")]
impl SteamPlatform {
    /// A stats-not-ready failure is the one case worth distinguishing —
    /// "not ready yet" is a transient state a caller can just retry once
    /// `stats_ready()` is true; anything else genuinely means the backend
    /// rejected the call.
    fn not_ready_reason(&self) -> String {
        if self.stats_ready() {
            "the Steam backend rejected the call".into()
        } else {
            "stats/achievements haven't finished loading from Steam yet".into()
        }
    }

    /// A mistyped achievement id is the single most common Steamworks
    /// partner-site foot-gun (`docs/steam-integration-proposal.md`) — name it
    /// explicitly once stats ARE known ready, since "not ready" is then ruled
    /// out and an unknown id is what's left.
    fn achievement_write_failed(&self, id: &str) -> String {
        if self.stats_ready() {
            format!("\"{id}\" isn't a known achievement id — check it against the Steamworks App Admin")
        } else {
            self.not_ready_reason()
        }
    }

    /// Same reasoning as [`achievement_write_failed`](Self::achievement_write_failed), for stats.
    fn stat_write_failed(&self, name: &str) -> String {
        if self.stats_ready() {
            format!("\"{name}\" isn't a known stat name — check it against the Steamworks App Admin")
        } else {
            self.not_ready_reason()
        }
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

    /// The batching boundary the whole "automatic flush" behavior hangs off:
    /// never on the very first pump before anything was ever tried, never
    /// before the interval has actually elapsed, always once it has —
    /// exactly on the boundary too, since `>=` (not `>`) is what makes
    /// `AUTO_FLUSH_INTERVAL` mean what it says.
    #[test]
    fn flush_is_due_respects_the_interval_boundary() {
        let now = Instant::now();
        let interval = Duration::from_secs(5);
        assert!(flush_is_due(None, now, interval), "nothing tried yet — always due");
        assert!(
            !flush_is_due(Some(now), now + Duration::from_secs(4), interval),
            "under the interval — not due yet"
        );
        assert!(
            flush_is_due(Some(now), now + Duration::from_secs(5), interval),
            "exactly the interval — due"
        );
        assert!(
            flush_is_due(Some(now), now + Duration::from_secs(6), interval),
            "past the interval — due"
        );
    }
}
