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
    Leaderboards, Platform, ScoreUploaded, Social, UploadMethod,
};

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
            lb_results: Arc::new(Mutex::new(Vec::new())),
            lb_boards: Arc::new(Mutex::new(HashMap::new())),
            lb_next_request: Cell::new(1),
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
    fn leaderboards(&self) -> Option<&dyn Leaderboards> {
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
