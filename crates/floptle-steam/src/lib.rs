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
//! [`floptle_services::Cloud`] (cloud saves) Phase 4,
//! [`floptle_services::Social`] (friends + presence) Phase 5a, and
//! [`floptle_services::Leaderboards`] Phase 9. Phase 6 adds `impl Transport
//! for SteamTransport`; the rest of `floptle_services`' sub-traits land as
//! their own phases give them methods to implement.
//!
//! **Leaderboards is where this crate grew its asynchronous half.** Steamworks
//! answers find/upload/download through a *call result* — a one-shot closure
//! Steam invokes from inside `run_callbacks()`, which is to say from inside
//! [`Platform::pump`]. That closure must be `Send + 'static`, so it cannot
//! borrow the client or touch anything Lua; it pushes plain data into
//! [`SteamPlatform`]'s own queue, and [`floptle_services::Leaderboards::poll`]
//! drains it on the next frame with the client back in hand. Any future
//! call-result-shaped surface (lobbies, Phase 6) should reuse that shape
//! rather than invent a second one.
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
use std::collections::HashMap;
#[cfg(feature = "steam")]
use std::sync::Mutex;

#[cfg(feature = "steam")]
use floptle_services::{
    Achievements, Cloud, FriendInfo, Identity, LeaderboardDisplay, LeaderboardEntry,
    LeaderboardInfo, LeaderboardOutcome, LeaderboardResult, LeaderboardScope, LeaderboardSort,
    Leaderboards, Lobbies, LobbyCompare, LobbyDistance, LobbyEvent, LobbyFilters, LobbyInfo,
    LobbyKind, LobbyMemberChange, LobbyOutcome, LobbyResult, Platform, ScoreUploaded, Social,
    UploadMethod,
};

/// The most members Steam will allow in one lobby. `steamworks`'
/// `create_lobby` **asserts** on anything larger — a panic, not an error — so
/// this is checked here before the call rather than after it.
#[cfg(feature = "steam")]
const MAX_LOBBY_MEMBERS: u32 = 250;

/// The longest a lobby data key may be. `steamworks`' `LobbyKey::new` panics
/// past this; checked here for the same reason as [`MAX_LOBBY_MEMBERS`].
#[cfg(feature = "steam")]
const MAX_LOBBY_KEY: usize = 255;

/// How many `details` ints to ask for per downloaded entry. Steam's own cap
/// is 64; asking for the maximum costs one stack buffer per entry and means a
/// caller never silently loses a payload another build uploaded.
#[cfg(feature = "steam")]
const MAX_DETAILS: usize = 64;

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
    /// Finished leaderboard requests waiting for the next
    /// [`Leaderboards::poll`]. `Mutex`, not `RefCell`, because Steamworks
    /// requires every call-result closure to be `Send` — the closure owns a
    /// clone of this `Arc`. In practice the lock is uncontended: Steam fires
    /// its call results from inside `run_callbacks()`, on the same thread
    /// that drains them.
    lb_results: Arc<Mutex<Vec<LeaderboardResult>>>,
    /// Every board handle resolved this session, keyed by its raw value.
    ///
    /// This registry is not a cache — it is load-bearing. `steamworks`'
    /// `Leaderboard` exposes `raw()` but has NO constructor from a raw value,
    /// so a handle that leaves Rust can never be turned back into one. Keeping
    /// the real values here is what lets a caller name a board with a plain
    /// number.
    lb_boards: Arc<Mutex<HashMap<u64, steamworks::Leaderboard>>>,
    /// The next request id. Monotonic for the whole life of this backend and
    /// never reset — which is why the script layer needs no generation
    /// counter to tell a stale result from a live one, unlike `http.*`: an id
    /// is never reused, so a result from a finished Play session can only
    /// find its own (already dropped) callback slot, never a new one.
    lb_next_request: Cell<u64>,
    /// Finished lobby requests, and lobby events, waiting to be polled. Same
    /// `Send` reasoning as [`lb_results`](Self::lb_results).
    lobby_results: Arc<Mutex<Vec<LobbyResult>>>,
    lobby_events: Arc<Mutex<Vec<LobbyEvent>>>,
    /// Why the last attempt to enter each lobby was refused.
    ///
    /// `join_lobby`'s own callback answers `Result<LobbyId, ()>` — an error
    /// with **no information in it at all**. The separate `LobbyEnter`
    /// callback carries the real reason (full, banned, doesn't exist…), so it
    /// is recorded here and used to explain a failed join. If it hasn't
    /// arrived yet the join still fails, just with a generic message —
    /// nothing waits on it.
    lobby_enter_errors: Arc<Mutex<HashMap<u64, &'static str>>>,
    /// Which lobby each in-flight join request was aimed at, so `poll` can
    /// pair a failure with the reason [`lobby_enter_errors`](Self::lobby_enter_errors)
    /// recorded for that same lobby. Read and cleared in `poll`.
    lobby_join_targets: Arc<Mutex<HashMap<u64, u64>>>,
    lobby_next_request: Cell<u64>,
    // Held only to keep the registrations alive — dropping one unregisters
    // its callback. Never read directly.
    _persona_cb: steamworks::CallbackHandle,
    _lobby_chat_update_cb: steamworks::CallbackHandle,
    _lobby_data_update_cb: steamworks::CallbackHandle,
    _lobby_enter_cb: steamworks::CallbackHandle,
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

        let lobby_events: Arc<Mutex<Vec<LobbyEvent>>> = Arc::new(Mutex::new(Vec::new()));

        let events = lobby_events.clone();
        let _lobby_chat_update_cb =
            client.register_callback::<steamworks::LobbyChatUpdate, _>(move |cb| {
                // `cb.making_change` is deliberately not read — see
                // `LobbyEvent::MemberChanged`; the binding fills it from the
                // wrong SDK field.
                lock(&events).push(LobbyEvent::MemberChanged {
                    lobby: cb.lobby.raw(),
                    user: cb.user_changed.raw(),
                    change: member_change_from_steam(cb.member_state_change),
                });
            });

        let events = lobby_events.clone();
        let _lobby_data_update_cb =
            client.register_callback::<steamworks::LobbyDataUpdate, _>(move |cb| {
                if cb.success {
                    lock(&events).push(LobbyEvent::DataChanged {
                        lobby: cb.lobby.raw(),
                        member: cb.member.raw(),
                    });
                }
            });

        let lobby_enter_errors: Arc<Mutex<HashMap<u64, &'static str>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let errors = lobby_enter_errors.clone();
        let _lobby_enter_cb = client.register_callback::<steamworks::LobbyEnter, _>(move |cb| {
            if let Some(why) = enter_refusal(cb.chat_room_enter_response) {
                lock(&errors).insert(cb.lobby.raw(), why);
            } else {
                lock(&errors).remove(&cb.lobby.raw());
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
            lb_results: Arc::new(Mutex::new(Vec::new())),
            lb_boards: Arc::new(Mutex::new(HashMap::new())),
            lb_next_request: Cell::new(1),
            lobby_results: Arc::new(Mutex::new(Vec::new())),
            lobby_events,
            lobby_enter_errors,
            lobby_join_targets: Arc::new(Mutex::new(HashMap::new())),
            lobby_next_request: Cell::new(1),
            _persona_cb,
            _lobby_chat_update_cb,
            _lobby_data_update_cb,
            _lobby_enter_cb,
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
    fn leaderboards(&self) -> Option<&dyn Leaderboards> {
        Some(self)
    }
    fn lobbies(&self) -> Option<&dyn Lobbies> {
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

/// Locks `m`, recovering from poisoning rather than propagating a panic.
///
/// Every critical section guarded by these mutexes is a `push`, a `take` or a
/// map insert — none of them can leave the data half-updated, so a lock
/// poisoned by an unrelated panic elsewhere still holds a perfectly valid
/// value. Answering `unwrap()` here would turn one panic into a second one
/// that loses a player's uploaded score.
#[cfg(feature = "steam")]
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(feature = "steam")]
fn sort_from_steam(s: steamworks::LeaderboardSortMethod) -> LeaderboardSort {
    match s {
        steamworks::LeaderboardSortMethod::Ascending => LeaderboardSort::Ascending,
        steamworks::LeaderboardSortMethod::Descending => LeaderboardSort::Descending,
    }
}

#[cfg(feature = "steam")]
fn sort_to_steam(s: LeaderboardSort) -> steamworks::LeaderboardSortMethod {
    match s {
        LeaderboardSort::Ascending => steamworks::LeaderboardSortMethod::Ascending,
        LeaderboardSort::Descending => steamworks::LeaderboardSortMethod::Descending,
    }
}

#[cfg(feature = "steam")]
fn display_from_steam(d: steamworks::LeaderboardDisplayType) -> LeaderboardDisplay {
    match d {
        steamworks::LeaderboardDisplayType::Numeric => LeaderboardDisplay::Numeric,
        steamworks::LeaderboardDisplayType::TimeSeconds => LeaderboardDisplay::TimeSeconds,
        steamworks::LeaderboardDisplayType::TimeMilliSeconds => {
            LeaderboardDisplay::TimeMilliseconds
        }
    }
}

#[cfg(feature = "steam")]
fn display_to_steam(d: LeaderboardDisplay) -> steamworks::LeaderboardDisplayType {
    match d {
        LeaderboardDisplay::Numeric => steamworks::LeaderboardDisplayType::Numeric,
        LeaderboardDisplay::TimeSeconds => steamworks::LeaderboardDisplayType::TimeSeconds,
        LeaderboardDisplay::TimeMilliseconds => {
            steamworks::LeaderboardDisplayType::TimeMilliSeconds
        }
    }
}

#[cfg(feature = "steam")]
fn scope_to_steam(s: LeaderboardScope) -> steamworks::LeaderboardDataRequest {
    match s {
        LeaderboardScope::Global => steamworks::LeaderboardDataRequest::Global,
        LeaderboardScope::GlobalAroundUser => {
            steamworks::LeaderboardDataRequest::GlobalAroundUser
        }
        LeaderboardScope::Friends => steamworks::LeaderboardDataRequest::Friends,
    }
}

#[cfg(feature = "steam")]
fn method_to_steam(m: UploadMethod) -> steamworks::UploadScoreMethod {
    match m {
        UploadMethod::KeepBest => steamworks::UploadScoreMethod::KeepBest,
        UploadMethod::ForceUpdate => steamworks::UploadScoreMethod::ForceUpdate,
    }
}

#[cfg(feature = "steam")]
fn member_change_from_steam(c: steamworks::ChatMemberStateChange) -> LobbyMemberChange {
    use steamworks::ChatMemberStateChange as C;
    match c {
        C::Entered => LobbyMemberChange::Entered,
        C::Left => LobbyMemberChange::Left,
        C::Disconnected => LobbyMemberChange::Disconnected,
        C::Kicked => LobbyMemberChange::Kicked,
        C::Banned => LobbyMemberChange::Banned,
    }
}

/// Why entering a lobby was refused, or `None` if it wasn't.
///
/// This exists because `join_lobby`'s own error is the unit type — it carries
/// nothing a player could act on. These strings are what turn "couldn't join"
/// into a sentence worth showing.
#[cfg(feature = "steam")]
fn enter_refusal(r: steamworks::ChatRoomEnterResponse) -> Option<&'static str> {
    use steamworks::ChatRoomEnterResponse as R;
    Some(match r {
        R::Success => return None,
        R::DoesntExist => "that lobby no longer exists",
        R::NotAllowed => "you aren't allowed into that lobby",
        R::Full => "that lobby is full",
        R::Banned => "you're banned from that lobby",
        R::Limited => "your Steam account is limited and can't join lobbies",
        R::ClanDisabled => "that group has been disabled",
        R::CommunityBan => "your Steam account has a community ban",
        R::MemberBlockedYou => "someone in that lobby has blocked you",
        R::YouBlockedMember => "you've blocked someone in that lobby",
        R::RatelimitExceeded => "you're joining lobbies too quickly — wait a moment",
        R::Error => "Steam refused the join",
    })
}

#[cfg(feature = "steam")]
fn lobby_kind_to_steam(k: LobbyKind) -> steamworks::LobbyType {
    match k {
        LobbyKind::Private => steamworks::LobbyType::Private,
        LobbyKind::FriendsOnly => steamworks::LobbyType::FriendsOnly,
        LobbyKind::Public => steamworks::LobbyType::Public,
        LobbyKind::Invisible => steamworks::LobbyType::Invisible,
    }
}

#[cfg(feature = "steam")]
fn distance_to_steam(d: LobbyDistance) -> steamworks::DistanceFilter {
    match d {
        LobbyDistance::Close => steamworks::DistanceFilter::Close,
        LobbyDistance::Default => steamworks::DistanceFilter::Default,
        LobbyDistance::Far => steamworks::DistanceFilter::Far,
        LobbyDistance::Worldwide => steamworks::DistanceFilter::Worldwide,
    }
}

#[cfg(feature = "steam")]
fn compare_to_steam(c: LobbyCompare) -> steamworks::ComparisonFilter {
    match c {
        LobbyCompare::Equal => steamworks::ComparisonFilter::Equal,
        LobbyCompare::NotEqual => steamworks::ComparisonFilter::NotEqual,
        LobbyCompare::Greater => steamworks::ComparisonFilter::GreaterThan,
        LobbyCompare::GreaterOrEqual => steamworks::ComparisonFilter::GreaterThanEqualTo,
        LobbyCompare::Less => steamworks::ComparisonFilter::LessThan,
        LobbyCompare::LessOrEqual => steamworks::ComparisonFilter::LessThanEqualTo,
    }
}

/// Refuse a lobby key/value the binding would PANIC on rather than reject.
///
/// `steamworks` builds a `CString` with `.unwrap()` and a `LobbyKey` with a
/// length assertion, so an interior NUL or an over-long key takes the whole
/// process down — and both are reachable straight from a script, where a Lua
/// string may contain any byte at all.
#[cfg(feature = "steam")]
fn check_lobby_key(key: &str, value: Option<&str>) -> Result<(), String> {
    if key.len() > MAX_LOBBY_KEY {
        return Err(format!(
            "a lobby data key can be at most {MAX_LOBBY_KEY} characters, and \"{}…\" is {}",
            &key[..32.min(key.len())],
            key.len()
        ));
    }
    if key.contains('\0') || value.is_some_and(|v| v.contains('\0')) {
        return Err("lobby data can't contain a zero byte".into());
    }
    Ok(())
}

#[cfg(feature = "steam")]
type Results = Arc<Mutex<Vec<LeaderboardResult>>>;
#[cfg(feature = "steam")]
type Boards = Arc<Mutex<HashMap<u64, steamworks::Leaderboard>>>;

#[cfg(feature = "steam")]
impl SteamPlatform {
    /// The next request id. See [`lb_next_request`](Self::lb_next_request)
    /// for why these are never reused.
    fn lb_next(&self) -> u64 {
        let id = self.lb_next_request.get();
        self.lb_next_request.set(id.wrapping_add(1));
        id
    }

    /// A live board by its raw handle, or `None` if this session never
    /// resolved it.
    fn lb_board(&self, id: u64) -> Option<steamworks::Leaderboard> {
        lock(&self.lb_boards).get(&id).cloned()
    }

    fn lb_push(results: &Results, request: u64, outcome: LeaderboardOutcome) {
        lock(results).push(LeaderboardResult { request, outcome });
    }

    /// The failure a caller gets for a board handle this session never
    /// resolved — by far the likeliest way to misuse this API, since a handle
    /// looks like an ordinary number that could have come from anywhere.
    fn lb_stale(results: &Results, request: u64, board: u64) {
        Self::lb_push(
            results,
            request,
            LeaderboardOutcome::Failed(format!(
                "{board} isn't a leaderboard handle from this session — call steam.findLeaderboard again (handles don't survive a restart)"
            )),
        );
    }

    /// Shared by `find` and `find_or_create`: register the resolved board and
    /// queue the result. Runs inside Steam's own callback, so it is `Send` and
    /// has no access to the client — the returned [`LeaderboardInfo`] carries
    /// only the id, and [`Self::fill_board_info`] completes it in `poll`.
    fn lb_deliver_board(
        results: &Results,
        boards: &Boards,
        request: u64,
        r: Result<Option<steamworks::Leaderboard>, steamworks::SteamError>,
    ) {
        let outcome = match r {
            Err(e) => LeaderboardOutcome::Failed(format!("Steam couldn't reach the leaderboard: {e}")),
            Ok(None) => LeaderboardOutcome::Board(None),
            Ok(Some(b)) => {
                let id = b.raw();
                lock(boards).insert(id, b);
                LeaderboardOutcome::Board(Some(LeaderboardInfo {
                    id,
                    name: String::new(),
                    entry_count: 0,
                    sort: None,
                    display: None,
                }))
            }
        };
        Self::lb_push(results, request, outcome);
    }

    /// Fill in the metadata Steam only answers synchronously, through the
    /// client the `Send` call-result closure could not capture.
    fn fill_board_info(&self, info: &mut LeaderboardInfo) {
        let Some(board) = self.lb_board(info.id) else {
            return;
        };
        let stats = self.client.user_stats();
        info.name = stats.get_leaderboard_name(&board);
        info.entry_count = stats.get_leaderboard_entry_count(&board);
        info.sort = stats.get_leaderboard_sort_method(&board).map(sort_from_steam);
        info.display = stats.get_leaderboard_display_type(&board).map(display_from_steam);
    }
}

#[cfg(feature = "steam")]
impl Leaderboards for SteamPlatform {
    fn find(&self, name: &str) -> u64 {
        let request = self.lb_next();
        let (results, boards) = (self.lb_results.clone(), self.lb_boards.clone());
        self.client.user_stats().find_leaderboard(name, move |r| {
            Self::lb_deliver_board(&results, &boards, request, r);
        });
        request
    }

    fn find_or_create(
        &self,
        name: &str,
        sort: LeaderboardSort,
        display: LeaderboardDisplay,
    ) -> u64 {
        let request = self.lb_next();
        let (results, boards) = (self.lb_results.clone(), self.lb_boards.clone());
        self.client.user_stats().find_or_create_leaderboard(
            name,
            sort_to_steam(sort),
            display_to_steam(display),
            move |r| Self::lb_deliver_board(&results, &boards, request, r),
        );
        request
    }

    fn upload(&self, board: u64, method: UploadMethod, score: i32, details: &[i32]) -> u64 {
        let request = self.lb_next();
        let Some(b) = self.lb_board(board) else {
            Self::lb_stale(&self.lb_results, request, board);
            return request;
        };
        let results = self.lb_results.clone();
        self.client.user_stats().upload_leaderboard_score(
            &b,
            method_to_steam(method),
            score,
            details,
            move |r| {
                let outcome = match r {
                    Err(e) => LeaderboardOutcome::Failed(format!("Steam rejected the score: {e}")),
                    // Steam reports a refused upload as a success carrying no
                    // payload; a caller that only checked for `Err` would
                    // otherwise read that as "uploaded".
                    Ok(None) => LeaderboardOutcome::Failed(
                        "Steam accepted the request but stored no score — the leaderboard may be read-only for this build".into(),
                    ),
                    Ok(Some(u)) => LeaderboardOutcome::Uploaded(ScoreUploaded {
                        score: u.score,
                        changed: u.was_changed,
                        global_rank_new: u.global_rank_new,
                        global_rank_previous: u.global_rank_previous,
                    }),
                };
                Self::lb_push(&results, request, outcome);
            },
        );
        request
    }

    fn download(&self, board: u64, scope: LeaderboardScope, start: i32, end: i32) -> u64 {
        let request = self.lb_next();
        let Some(b) = self.lb_board(board) else {
            Self::lb_stale(&self.lb_results, request, board);
            return request;
        };
        let results = self.lb_results.clone();
        // `start`/`end` are signed on purpose and the binding takes `usize`:
        // an around-user request uses NEGATIVE ranks for "better than me", and
        // the binding casts straight back down to a C `int`. The sign-extend
        // out and truncate back in round-trips exactly, which is the only
        // reason passing a negative rank through a `usize` is correct here.
        self.client.user_stats().download_leaderboard_entries(
            &b,
            scope_to_steam(scope),
            start as usize,
            end as usize,
            MAX_DETAILS,
            move |r| {
                let outcome = match r {
                    Err(e) => LeaderboardOutcome::Failed(format!(
                        "Steam couldn't download the leaderboard: {e}"
                    )),
                    Ok(entries) => LeaderboardOutcome::Entries(
                        entries
                            .into_iter()
                            .map(|e| LeaderboardEntry {
                                user_id: e.user.raw(),
                                global_rank: e.global_rank,
                                score: e.score,
                                details: e.details,
                            })
                            .collect(),
                    ),
                };
                Self::lb_push(&results, request, outcome);
            },
        );
        request
    }

    fn poll(&self) -> Vec<LeaderboardResult> {
        let mut out = std::mem::take(&mut *lock(&self.lb_results));
        for r in &mut out {
            if let LeaderboardOutcome::Board(Some(info)) = &mut r.outcome {
                self.fill_board_info(info);
            }
        }
        out
    }
}

#[cfg(feature = "steam")]
type LobbyQueue = Arc<Mutex<Vec<LobbyResult>>>;

#[cfg(feature = "steam")]
impl SteamPlatform {
    fn lobby_next(&self) -> u64 {
        let id = self.lobby_next_request.get();
        self.lobby_next_request.set(id.wrapping_add(1));
        id
    }

    fn lobby_push(q: &LobbyQueue, request: u64, outcome: LobbyOutcome) {
        lock(q).push(LobbyResult { request, outcome });
    }

    /// Read everything about a lobby that is answerable synchronously. Used
    /// to complete a create/join/list result in `poll`, for the same reason
    /// as [`Self::fill_board_info`]: the call-result closure is `Send` and
    /// cannot reach the client.
    fn lobby_info(&self, id: u64) -> LobbyInfo {
        let mm = self.client.matchmaking();
        let lobby = steamworks::LobbyId::from_raw(id);
        let count = mm.lobby_data_count(lobby);
        let data = (0..count).filter_map(|i| mm.lobby_data_by_index(lobby, i)).collect();
        let owner = mm.lobby_owner(lobby).raw();
        LobbyInfo {
            id,
            member_count: mm.lobby_member_count(lobby),
            member_limit: mm.lobby_member_limit(lobby),
            // Steam answers 0 for a lobby it knows nothing about, which is
            // not a Steam id anybody has.
            owner: (owner != 0).then_some(owner),
            data,
        }
    }

    /// The reason the last enter attempt on `id` was refused, consumed so a
    /// stale one can't explain a later, different failure.
    fn take_enter_error(&self, id: u64) -> Option<&'static str> {
        lock(&self.lobby_enter_errors).remove(&id)
    }
}

#[cfg(feature = "steam")]
impl Lobbies for SteamPlatform {
    fn create(&self, kind: LobbyKind, max_members: u32) -> u64 {
        let request = self.lobby_next();
        // `steamworks::create_lobby` ASSERTS on this rather than returning an
        // error, so checking after the call is checking after the panic.
        if max_members == 0 || max_members > MAX_LOBBY_MEMBERS {
            Self::lobby_push(
                &self.lobby_results,
                request,
                LobbyOutcome::Failed(format!(
                    "a lobby holds 1 to {MAX_LOBBY_MEMBERS} members, not {max_members}"
                )),
            );
            return request;
        }
        let results = self.lobby_results.clone();
        self.client.matchmaking().create_lobby(
            lobby_kind_to_steam(kind),
            max_members,
            move |r| {
                let outcome = match r {
                    Ok(id) => LobbyOutcome::Created(LobbyInfo {
                        id: id.raw(),
                        member_count: 0,
                        member_limit: None,
                        owner: None,
                        data: Vec::new(),
                    }),
                    Err(e) => LobbyOutcome::Failed(format!("Steam couldn't create a lobby: {e}")),
                };
                Self::lobby_push(&results, request, outcome);
            },
        );
        request
    }

    fn join(&self, lobby: u64) -> u64 {
        let request = self.lobby_next();
        lock(&self.lobby_join_targets).insert(request, lobby);
        let results = self.lobby_results.clone();
        self.client.matchmaking().join_lobby(
            steamworks::LobbyId::from_raw(lobby),
            move |r| {
                let outcome = match r {
                    Ok(id) => LobbyOutcome::Joined(LobbyInfo {
                        id: id.raw(),
                        member_count: 0,
                        member_limit: None,
                        owner: None,
                        data: Vec::new(),
                    }),
                    // The real reason is filled in by `poll`, which can reach
                    // the `LobbyEnter` callback's record; this closure can't.
                    Err(()) => LobbyOutcome::Failed(String::new()),
                };
                Self::lobby_push(&results, request, outcome);
            },
        );
        request
    }

    fn list(&self, filters: &LobbyFilters) -> u64 {
        let request = self.lobby_next();
        let mm = self.client.matchmaking();
        for (k, v) in &filters.string {
            if check_lobby_key(k, Some(v)).is_err() {
                continue; // a key Steam would panic on simply matches nothing
            }
            mm.add_request_lobby_list_string_filter(steamworks::StringFilter(
                steamworks::LobbyKey::new(k),
                v,
                // Explicitly `Equal`: this enum's DEFAULT is
                // `EqualToOrLessThan`, i.e. a lexicographic `<=`, which is
                // not what anyone filtering on a game mode means.
                steamworks::StringFilterKind::Equal,
            ));
        }
        for (k, v, how) in &filters.number {
            if check_lobby_key(k, None).is_err() {
                continue;
            }
            mm.add_request_lobby_list_numerical_filter(steamworks::NumberFilter(
                steamworks::LobbyKey::new(k),
                *v,
                compare_to_steam(*how),
            ));
        }
        if let Some(slots) = filters.slots_available {
            mm.set_request_lobby_list_slots_available_filter(slots);
        }
        if let Some(d) = filters.distance {
            mm.set_request_lobby_list_distance_filter(distance_to_steam(d));
        }
        if let Some(n) = filters.max_results {
            mm.set_request_lobby_list_result_count_filter(n);
        }
        let results = self.lobby_results.clone();
        mm.request_lobby_list(move |r| {
            let outcome = match r {
                Ok(ids) => LobbyOutcome::Listed(
                    ids.into_iter()
                        .map(|id| LobbyInfo {
                            id: id.raw(),
                            member_count: 0,
                            member_limit: None,
                            owner: None,
                            data: Vec::new(),
                        })
                        .collect(),
                ),
                Err(e) => LobbyOutcome::Failed(format!("Steam couldn't search for lobbies: {e}")),
            };
            Self::lobby_push(&results, request, outcome);
        });
        request
    }

    fn leave(&self, lobby: u64) {
        self.client.matchmaking().leave_lobby(steamworks::LobbyId::from_raw(lobby));
    }

    fn data(&self, lobby: u64, key: &str) -> Option<String> {
        check_lobby_key(key, None).ok()?;
        self.client.matchmaking().lobby_data(steamworks::LobbyId::from_raw(lobby), key)
    }

    fn all_data(&self, lobby: u64) -> Vec<(String, String)> {
        let mm = self.client.matchmaking();
        let lobby = steamworks::LobbyId::from_raw(lobby);
        (0..mm.lobby_data_count(lobby)).filter_map(|i| mm.lobby_data_by_index(lobby, i)).collect()
    }

    fn set_data(&self, lobby: u64, key: &str, value: &str) -> Result<(), String> {
        check_lobby_key(key, Some(value))?;
        if self.client.matchmaking().set_lobby_data(steamworks::LobbyId::from_raw(lobby), key, value)
        {
            Ok(())
        } else {
            Err(format!(
                "Steam refused to set \"{key}\" — only a lobby's owner can change its data, \
                 and only while they're still in it"
            ))
        }
    }

    fn delete_data(&self, lobby: u64, key: &str) -> Result<(), String> {
        check_lobby_key(key, None)?;
        if self.client.matchmaking().delete_lobby_data(steamworks::LobbyId::from_raw(lobby), key) {
            Ok(())
        } else {
            Err(format!("Steam refused to delete \"{key}\" — owner only, and it must exist"))
        }
    }

    fn member_data(&self, lobby: u64, member: u64, key: &str) -> Option<String> {
        check_lobby_key(key, None).ok()?;
        let got = self.client.matchmaking().get_lobby_member_data(
            steamworks::LobbyId::from_raw(lobby),
            steamworks::SteamId::from_raw(member),
            key,
        );
        // Steam answers an empty string for "no such key", which is
        // indistinguishable from a key genuinely set to "". `None` is the
        // honest answer for both.
        got.filter(|s| !s.is_empty())
    }

    fn set_member_data(&self, lobby: u64, key: &str, value: &str) -> Result<(), String> {
        check_lobby_key(key, Some(value))?;
        self.client.matchmaking().set_lobby_member_data(
            steamworks::LobbyId::from_raw(lobby),
            key,
            value,
        );
        Ok(())
    }

    fn members(&self, lobby: u64) -> Vec<u64> {
        self.client
            .matchmaking()
            .lobby_members(steamworks::LobbyId::from_raw(lobby))
            .into_iter()
            .map(|m| m.raw())
            .collect()
    }

    fn owner(&self, lobby: u64) -> Option<u64> {
        let owner = self.client.matchmaking().lobby_owner(steamworks::LobbyId::from_raw(lobby)).raw();
        (owner != 0).then_some(owner)
    }

    fn member_limit(&self, lobby: u64) -> Option<usize> {
        self.client.matchmaking().lobby_member_limit(steamworks::LobbyId::from_raw(lobby))
    }

    fn set_joinable(&self, lobby: u64, joinable: bool) -> Result<(), String> {
        if self
            .client
            .matchmaking()
            .set_lobby_joinable(steamworks::LobbyId::from_raw(lobby), joinable)
        {
            Ok(())
        } else {
            Err("Steam refused — only a lobby's owner can open or close it".into())
        }
    }

    fn poll(&self) -> Vec<LobbyResult> {
        let mut out = std::mem::take(&mut *lock(&self.lobby_results));
        for r in &mut out {
            // Whatever happened, this request is no longer in flight.
            let target = lock(&self.lobby_join_targets).remove(&r.request);
            match &mut r.outcome {
                // Fill in everything the `Send` closure could not read.
                LobbyOutcome::Created(info) | LobbyOutcome::Joined(info) => {
                    *info = self.lobby_info(info.id);
                }
                LobbyOutcome::Listed(list) => {
                    for info in list.iter_mut() {
                        *info = self.lobby_info(info.id);
                    }
                }
                // A join failure arrives with an EMPTY message on purpose:
                // Steam's own join error is the unit type and carries no
                // reason at all. The reason lives in the `LobbyEnter`
                // callback's record, and by now `pump` has run every callback
                // this frame — so whichever of the two Steam dispatched first,
                // both have landed and the pairing is safe here in a way it
                // would not be inside either callback.
                LobbyOutcome::Failed(why) if why.is_empty() => {
                    *why = match target.and_then(|id| self.take_enter_error(id)) {
                        Some(reason) => reason.to_string(),
                        None => "couldn't join that lobby".into(),
                    };
                }
                LobbyOutcome::Failed(_) => {}
            }
        }
        out
    }

    fn poll_events(&self) -> Vec<LobbyEvent> {
        std::mem::take(&mut *lock(&self.lobby_events))
    }
}

#[cfg(all(test, feature = "steam"))]
mod tests {
    use super::*;

    /// `init` answers a plain `Result` either way, and never panics — which
    /// is what lets a caller fall back to [`floptle_services::NullPlatform`]
    /// rather than crash.
    ///
    /// **This deliberately does not assert WHICH way.** It used to assert
    /// `is_err()`, on the reasoning that no Steam client runs in CI — true
    /// there, and false on the machine of anybody actually developing this
    /// crate, who has Steam open. App 480 (Spacewar) is free to every Steam
    /// account, so with a client running `SteamAPI_Init` genuinely SUCCEEDS
    /// and the old assertion failed. Worse, it failed by panicking mid-test:
    /// unwinding dropped a live client, whose `SteamAPI_Shutdown` then
    /// deadlocked against the callback thread and hung the whole suite in
    /// `futex_wait` — a test-suite hang whose cause looks nothing like "an
    /// assertion about an unrelated thing was environment-dependent".
    ///
    /// The environment-independent claim is the one the caller actually
    /// relies on, so that is what this asserts.
    #[test]
    fn init_answers_a_result_either_way_and_never_panics() {
        // Dropping an `Ok` here shuts Steam back down; that is fine, and
        // deliberately the last thing this test does.
        match SteamPlatform::init(480) {
            Ok(p) => assert!(p.available(), "a live backend must report itself available"),
            Err(_) => { /* no client running — equally correct */ }
        }
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
    /// Every leaderboard enum survives a round trip through Steam's own.
    ///
    /// A swapped arm here is invisible: the call succeeds, the board is real,
    /// and the scores are simply ranked the wrong way round — or a lap time
    /// is displayed as a point score. Nothing errors, and the only symptom is
    /// a leaderboard that looks wrong to players. The round trip is what
    /// makes a swap fail HERE instead.
    #[test]
    fn every_leaderboard_enum_survives_a_round_trip_through_steams_own() {
        for s in [LeaderboardSort::Ascending, LeaderboardSort::Descending] {
            assert_eq!(sort_from_steam(sort_to_steam(s)), s, "{s:?} did not round-trip");
        }
        for d in [
            LeaderboardDisplay::Numeric,
            LeaderboardDisplay::TimeSeconds,
            LeaderboardDisplay::TimeMilliseconds,
        ] {
            assert_eq!(display_from_steam(display_to_steam(d)), d, "{d:?} did not round-trip");
        }
    }

    /// Steam has no reverse mapping for these two (and `LeaderboardDataRequest`
    /// carries no derives at all, not even `Debug`), so a round trip can't
    /// guard them — pin each arm to the variant it must produce instead.
    ///
    /// Swapping `KeepBest` and `ForceUpdate` is the one that costs a player
    /// something real: a worse run would overwrite their best score, and the
    /// call would report success doing it.
    #[test]
    fn scope_and_method_map_to_the_matching_steam_variant() {
        use steamworks::{LeaderboardDataRequest as Req, UploadScoreMethod as Method};
        assert!(matches!(scope_to_steam(LeaderboardScope::Global), Req::Global));
        assert!(matches!(
            scope_to_steam(LeaderboardScope::GlobalAroundUser),
            Req::GlobalAroundUser
        ));
        assert!(matches!(scope_to_steam(LeaderboardScope::Friends), Req::Friends));

        assert!(matches!(method_to_steam(UploadMethod::KeepBest), Method::KeepBest));
        assert!(matches!(method_to_steam(UploadMethod::ForceUpdate), Method::ForceUpdate));
    }

    /// The two lobby inputs `steamworks` PANICS on rather than rejecting must
    /// be refused before the call.
    ///
    /// `LobbyKey::new` asserts on a key past 255 bytes, and the binding builds
    /// its `CString` with `.unwrap()`, so an interior NUL aborts too. Both
    /// arrive straight from a script — a Lua string holds any byte — so
    /// without this guard a game could take the whole engine down with one
    /// `steam.setLobbyData` call.
    #[test]
    fn lobby_keys_steamworks_would_panic_on_are_refused_first() {
        assert!(check_lobby_key("mode", Some("coop")).is_ok());

        let long = "k".repeat(MAX_LOBBY_KEY + 1);
        let e = check_lobby_key(&long, None).unwrap_err();
        assert!(e.contains(&MAX_LOBBY_KEY.to_string()), "should state the limit: {e}");

        assert!(check_lobby_key("mo\0de", None).is_err(), "an interior NUL in the key");
        assert!(check_lobby_key("mode", Some("co\0op")).is_err(), "or in the value");

        // Exactly at the limit is fine — an off-by-one here would refuse a
        // key Steam accepts.
        assert!(check_lobby_key(&"k".repeat(MAX_LOBBY_KEY), None).is_ok());
    }

    /// Every enter refusal maps to a sentence worth showing a player, and
    /// success maps to no error at all.
    ///
    /// This is the whole reason the `LobbyEnter` callback is registered:
    /// `join_lobby`'s own error is the unit type and says nothing.
    #[test]
    fn every_enter_refusal_has_a_reason_and_success_has_none() {
        use steamworks::ChatRoomEnterResponse as R;
        assert_eq!(enter_refusal(R::Success), None, "success is not a refusal");
        for r in [
            R::DoesntExist,
            R::NotAllowed,
            R::Full,
            R::Banned,
            R::Limited,
            R::ClanDisabled,
            R::CommunityBan,
            R::MemberBlockedYou,
            R::YouBlockedMember,
            R::RatelimitExceeded,
            R::Error,
        ] {
            let why = enter_refusal(r).unwrap_or_else(|| panic!("{r:?} has no reason"));
            assert!(!why.is_empty(), "{r:?}");
        }
    }

    /// Every lobby enum maps to the matching Steam variant. `LobbyType` and
    /// `DistanceFilter` have no reverse mapping, so each arm is pinned.
    #[test]
    fn lobby_enums_map_to_the_matching_steam_variant() {
        use steamworks::{DistanceFilter as D, LobbyType as T};
        assert!(matches!(lobby_kind_to_steam(LobbyKind::Public), T::Public));
        assert!(matches!(lobby_kind_to_steam(LobbyKind::Private), T::Private));
        assert!(matches!(lobby_kind_to_steam(LobbyKind::FriendsOnly), T::FriendsOnly));
        assert!(matches!(lobby_kind_to_steam(LobbyKind::Invisible), T::Invisible));

        assert!(matches!(distance_to_steam(LobbyDistance::Close), D::Close));
        assert!(matches!(distance_to_steam(LobbyDistance::Default), D::Default));
        assert!(matches!(distance_to_steam(LobbyDistance::Far), D::Far));
        assert!(matches!(distance_to_steam(LobbyDistance::Worldwide), D::Worldwide));

        // Swapping Greater and Less here would silently invert a skill-based
        // matchmaking filter — the search still works and finds the wrong
        // players.
        use steamworks::ComparisonFilter as C;
        assert!(matches!(compare_to_steam(LobbyCompare::Equal), C::Equal));
        assert!(matches!(compare_to_steam(LobbyCompare::NotEqual), C::NotEqual));
        assert!(matches!(compare_to_steam(LobbyCompare::Greater), C::GreaterThan));
        assert!(matches!(
            compare_to_steam(LobbyCompare::GreaterOrEqual),
            C::GreaterThanEqualTo
        ));
        assert!(matches!(compare_to_steam(LobbyCompare::Less), C::LessThan));
        assert!(matches!(compare_to_steam(LobbyCompare::LessOrEqual), C::LessThanEqualTo));
    }

    /// A poisoned lock must still hand back the data. These mutexes guard a
    /// `push` and a `take`, neither of which can leave a half-written value,
    /// so panicking a second time would only lose a score somebody earned.
    #[test]
    fn a_poisoned_lock_still_yields_its_value() {
        let m = Arc::new(Mutex::new(vec![1i32, 2, 3]));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison it");
        })
        .join();
        assert!(m.is_poisoned(), "the lock should be poisoned for this test to mean anything");
        assert_eq!(*lock(&m), vec![1, 2, 3]);
    }

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
