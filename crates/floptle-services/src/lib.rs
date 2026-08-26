//! # floptle-services — the platform capability boundary
//!
//! Phase 0 of the Steam integration plan. [`Platform`] is the one
//! trait every downstream crate depends on; a platform SDK's own types
//! (Steamworks today, in [`floptle_steam`](../floptle_steam/index.html), gated
//! behind its `steam` feature) never reach `floptle-script`,
//! `floptle-runtime`, or `floptle-editor` directly.
//!
//! Shape mirrors `floptle_net::Transport`: one small trait per concern,
//! composed, with an always-available no-dependency default —
//! [`NullPlatform`] here, `MemoryTransport` there. Each sub-trait
//! ([`Achievements`], [`Cloud`], [`Identity`], [`Entitlements`], [`Ugc`],
//! [`Overlay`], [`Input`], [`Social`], [`Leaderboards`]) starts empty on
//! purpose: its methods land with the phase that actually needs them
//! (Achievements in Phase 2, Overlay in Phase 3, and so on), so this crate's
//! job is the boundary shape, not a guessed-ahead capability surface.
//!
//! [`Leaderboards`] is the one sub-trait Phase 0 did not name. It got its own
//! rather than being folded into [`Achievements`]: that trait absorbed stats
//! because *Steamworks itself* puts both behind one interface, which is a
//! reason about the backend rather than about the name — and no equivalent
//! reason makes a leaderboard an achievement.
//!
//! [`NullPlatform`] is meant to be the default across the whole workspace
//! test suite: a project with no `steam` project setting, an in-editor Play
//! session, or a headless test all run against it, so nothing here needs a
//! platform SDK present to compile or to pass.

#![warn(missing_docs)]

/// Achievement unlock/query + int/float stats. Landed Phase 2 — Steamworks
/// groups both under one interface (`ISteamUserStats`), and so does this
/// trait, rather than inventing a category Phase 0 didn't name.
///
/// **Average-rate stats are out of scope.** The Steamworks binding this
/// engine uses doesn't wrap `UpdateAvgRateStat`/its `GetStatValue` variant at
/// all — a real gap, not an oversight; see
/// the Steam integration plan. **So is progress-indicator
/// notifications** (`IndicateAchievementProgress`) — also unbound. Both would
/// need raw FFI to close.
pub trait Achievements {
    /// Whether stats/achievements have finished loading from the backend —
    /// every other method here answers honestly (`None`/`Err`, never a
    /// guess) until this is `true`.
    fn stats_ready(&self) -> bool;
    /// Is `id` unlocked? `None` when stats aren't ready yet, or `id` isn't a
    /// real achievement — a mistyped id is the single most common
    /// Steamworks-backend misconfiguration.
    fn achievement_unlocked(&self, id: &str) -> Option<bool>;
    /// Unlocks `id` LOCALLY — cheap, in-memory. Reaches the backend's server
    /// (and triggers its native unlock notification) on the next automatic
    /// batch or an explicit [`flush`](Self::flush). `Err`'s message is
    /// actionable — a mistyped id says so, rather than a bare failure.
    fn unlock_achievement(&self, id: &str) -> Result<(), String>;
    /// Resets `id` to locked, locally — same batching as
    /// [`unlock_achievement`](Self::unlock_achievement).
    fn clear_achievement(&self, id: &str) -> Result<(), String>;
    /// The percentage of players globally who have unlocked `id`, once the
    /// backend has this cached (`None` before then).
    fn achievement_global_percent(&self, id: &str) -> Option<f32>;
    /// `id`'s display name, in the backend's own current language.
    fn achievement_name(&self, id: &str) -> Option<String>;
    /// `id`'s display description, in the backend's own current language.
    fn achievement_description(&self, id: &str) -> Option<String>;

    /// Reads an integer stat. `None` before stats are ready or if `name`
    /// isn't a real stat.
    fn stat_int(&self, name: &str) -> Option<i32>;
    /// Writes an integer stat LOCALLY — same batching as achievement writes.
    fn set_stat_int(&self, name: &str, value: i32) -> Result<(), String>;
    /// Reads a float stat.
    fn stat_float(&self, name: &str) -> Option<f32>;
    /// Writes a float stat LOCALLY.
    fn set_stat_float(&self, name: &str, value: f32) -> Result<(), String>;

    /// Sends every pending achievement/stat write to the backend now, rather
    /// than waiting for the next automatic batch. Safe to call with nothing
    /// pending (a no-op). A failed send (offline, a transient backend error)
    /// is NOT lost — it stays queued and the next automatic batch (or the
    /// next explicit `flush`) retries it.
    fn flush(&self);
    /// Wipes every stat, and every achievement if `achievements_too` — for
    /// development/QA, never for a shipping build's own use.
    fn reset_all_stats(&self, achievements_too: bool) -> Result<(), String>;
}

/// Cloud save read/write/enumerate surface. Landed Phase 4.
///
/// **Quota reporting is out of scope.** The Steamworks binding this engine
/// uses doesn't wrap `GetQuota` at all — a real gap, not an oversight; see
/// the Steam integration plan.
///
/// **Conflict policy is the caller's to build, on purpose.** Steam Cloud has
/// no built-in multi-writer conflict concept to expose — [`file_timestamp`]
/// is the primitive a caller compares against its own local save's
/// modification time to decide what "newer" means for itself, rather than
/// this trait silently picking a winner.
pub trait Cloud {
    /// Whether Cloud is enabled for this app specifically (independent of
    /// the account-wide setting).
    fn is_enabled_for_app(&self) -> bool;
    /// Toggles [`is_enabled_for_app`](Self::is_enabled_for_app).
    fn set_enabled_for_app(&self, enabled: bool);
    /// Whether Cloud is enabled account-wide (independent of the per-app
    /// setting) — read-only: a player controls this from the Steam client
    /// itself, not from inside a game.
    fn is_enabled_for_account(&self) -> bool;
    /// Every file currently in Cloud storage for this app, as `(name, size in
    /// bytes)`.
    fn files(&self) -> Vec<(String, u64)>;
    /// Whether `name` exists in Cloud storage. The file needn't exist to be
    /// named in any other call here — `write_file` creates it.
    fn file_exists(&self, name: &str) -> bool;
    /// `name`'s last-write timestamp (Unix seconds), if it exists.
    fn file_timestamp(&self, name: &str) -> Option<i64>;
    /// Deletes `name` locally AND remotely. `false` if there was nothing to
    /// delete.
    fn delete_file(&self, name: &str) -> Result<(), String>;
    /// Deletes `name` from the Cloud while keeping the local copy — for a
    /// player who wants this specific save to stop syncing without losing it.
    fn forget_file(&self, name: &str) -> Result<(), String>;
    /// Reads `name`'s full contents.
    fn read_file(&self, name: &str) -> Result<Vec<u8>, String>;
    /// Writes `data` as `name`'s full contents, replacing whatever was there.
    fn write_file(&self, name: &str, data: &[u8]) -> Result<(), String>;
}

/// Identity of the local user and the running app/build. Landed Phase 1.
pub trait Identity {
    /// The signed-in local user's platform-account id (a Steam64 id, on the
    /// Steam backend).
    fn local_user_id(&self) -> u64;
    /// The local user's current persona (display) name.
    fn persona_name(&self) -> String;
    /// A 32×32 RGBA8 avatar for the local user, if the backend has one cached.
    fn avatar_small(&self) -> Option<Vec<u8>>;
    /// A 64×64 RGBA8 avatar for the local user, if the backend has one cached.
    fn avatar_medium(&self) -> Option<Vec<u8>>;
    /// A 184×184 RGBA8 avatar for the local user, if the backend has one cached.
    fn avatar_large(&self) -> Option<Vec<u8>>;
    /// `true` since the last poll if the local user's persona (name or
    /// avatar) changed — a drain, not a push, matching the engine's per-frame
    /// callback-drain pattern (the Steam integration plan).
    fn poll_persona_change(&self) -> bool;
    /// This build's build id, as the backend reports it.
    fn build_id(&self) -> i32;
    /// This app's install directory, as the backend reports it.
    fn install_dir(&self) -> String;
    /// The beta branch this build was installed from, if any (not the
    /// default branch).
    fn beta_name(&self) -> Option<String>;
    /// `true` if this app is being played on a license borrowed from another
    /// account (Steam Family Sharing), not one the signed-in user owns.
    fn is_family_shared(&self) -> bool;
    /// `true` if the backend has flagged this as a cybercafe/shared-computer
    /// license.
    fn is_cybercafe(&self) -> bool;
    /// The backend UI's current language (e.g. `"english"`, `"french"`) — a
    /// reasonable default for the engine's own localization, landed Phase 13.
    fn ui_language(&self) -> String;
    /// `true` if the backend reports this session as running on its own
    /// handheld hardware (Steam Deck). No physical keyboard/mouse should be
    /// assumed when this is `true`.
    fn is_steam_deck(&self) -> bool;
    /// `true` if the backend's own "10-foot" full-screen mode (Big Picture)
    /// is active.
    fn is_big_picture_mode(&self) -> bool;
}

/// DLC/entitlement ownership surface. Empty until Phase 8.
pub trait Entitlements {}

/// Workshop/UGC item surface. Empty until Phase 10.
pub trait Ugc {}

/// Overlay page-open / activation-event surface. Empty until Phase 3.
pub trait Overlay {}

/// Platform-specific controller input (action sets, glyphs, haptics). Empty
/// until Phase 7 — distinct from `floptle_input`, which already owns
/// device-agnostic action mapping; this is the per-platform layer beneath it.
pub trait Input {}

/// One friend, as the local user's backend reports them.
#[derive(Debug, Clone, PartialEq)]
pub struct FriendInfo {
    /// Their platform-account id (a Steam64 id, on the Steam backend) — a
    /// `u64` here; convert to a string at any boundary where exactness past
    /// 2^53 matters (Lua numbers, JSON), same reason as
    /// [`Identity::local_user_id`].
    pub id: u64,
    /// Their current display name.
    pub name: String,
    /// Their online state, lowercase (`"online"`, `"away"`, `"busy"`,
    /// `"snooze"`, `"looking to trade"`, `"looking to play"`,
    /// `"invisible"`, `"offline"`) — a plain string rather than a typed enum
    /// on purpose: this is read-only, display-shaped data, and the backend's
    /// own state list is unlikely to gain new values a caller must switch
    /// on exhaustively.
    pub state: String,
    /// `true` if they're currently playing THIS app (not just online).
    pub playing_this_game: bool,
}

/// Friends and presence. Landed Phase 5a — **not** invites/join (5b, which
/// needs Phase 6's transport to route a cold launch into the right session)
/// and **not** the overlay dialogs that open a friend/invite UI (those are
/// `Overlay`/Phase 3 calls, gated on the swapchain spike the source spec
/// calls a go/no-go check before that phase lands).
///
/// **Group/clan membership is out of scope.** The Steamworks binding this
/// engine uses doesn't wrap clan enumeration at all — a real gap, not an
/// oversight; see the Steam integration plan.
pub trait Social {
    /// Sets a rich-presence key for the local user, visible to friends in
    /// their friend list — e.g. `("status", "In the lobby")`. The backend
    /// caps the number of keys and their length; `Err` names the reason
    /// (an unknown/malformed key, or the cap reached), not a bare failure.
    fn set_rich_presence(&self, key: &str, value: &str) -> Result<(), String>;
    /// Clears every rich-presence key set via
    /// [`set_rich_presence`](Self::set_rich_presence).
    fn clear_rich_presence(&self);
    /// The local user's friend list.
    fn friends(&self) -> Vec<FriendInfo>;
    /// Reads one of `friend_id`'s OWN rich-presence keys (set via their
    /// own `set_rich_presence`) — `None` if they haven't set it, aren't a
    /// friend, or aren't currently in a session the backend can read it
    /// from.
    fn friend_rich_presence(&self, friend_id: u64, key: &str) -> Option<String>;
}

/// Which direction a leaderboard ranks scores — fixed when the board is
/// created and not changeable afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaderboardSort {
    /// A lower score ranks better (lap times, stroke counts).
    Ascending,
    /// A higher score ranks better (points, kills, distance).
    Descending,
}

/// How a leaderboard's scores should be formatted for display. The backend
/// stores every score as a plain `i32` regardless — this is presentation
/// metadata a caller reads to format it, not a change of storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaderboardDisplay {
    /// A plain number.
    Numeric,
    /// The score is a count of seconds.
    TimeSeconds,
    /// The score is a count of milliseconds.
    TimeMilliseconds,
}

/// Which slice of a leaderboard to download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaderboardScope {
    /// Ranks counted from the top of the board.
    Global,
    /// Ranks counted RELATIVE to the local user's own — a negative start and
    /// a positive end give the rows either side of them.
    GlobalAroundUser,
    /// Only the local user's friends, ranks counted from the best of them.
    Friends,
}

/// What to do when a score is uploaded for a user who already has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadMethod {
    /// Keep whichever score ranks better under the board's own sort. The
    /// right choice for a high-score board.
    KeepBest,
    /// Overwrite unconditionally, even with a worse score. The right choice
    /// for a "most recent run" board.
    ForceUpdate,
}

/// A leaderboard the backend has resolved, with the metadata its synchronous
/// getters answer once the handle exists.
#[derive(Debug, Clone, PartialEq)]
pub struct LeaderboardInfo {
    /// The backend's own handle for this board. Opaque — meaningful only to
    /// the backend that issued it, and only for as long as this session
    /// lives; do NOT save it and expect it to resolve next time.
    pub id: u64,
    /// The board's name, as it was created on the backend.
    pub name: String,
    /// How many entries the board holds in total.
    pub entry_count: i32,
    /// The board's sort direction, if the backend reports one.
    pub sort: Option<LeaderboardSort>,
    /// The board's display formatting, if the backend reports one.
    pub display: Option<LeaderboardDisplay>,
}

/// One row of a downloaded leaderboard.
#[derive(Debug, Clone, PartialEq)]
pub struct LeaderboardEntry {
    /// The entry owner's platform-account id — a `u64` for the same reason as
    /// [`Identity::local_user_id`]; convert to a string at any boundary where
    /// exactness past 2^53 matters.
    pub user_id: u64,
    /// Their rank on the board, 1-based.
    pub global_rank: i32,
    /// Their score.
    pub score: i32,
    /// The opaque per-entry payload uploaded alongside the score (ghost data,
    /// a replay seed, a loadout). Empty when none was uploaded.
    pub details: Vec<i32>,
}

/// What came back from an [`Leaderboards::upload`].
#[derive(Debug, Clone, PartialEq)]
pub struct ScoreUploaded {
    /// The score now stored — NOT necessarily the one uploaded, under
    /// [`UploadMethod::KeepBest`].
    pub score: i32,
    /// Whether this upload actually changed the stored score.
    pub changed: bool,
    /// The user's rank after the upload.
    pub global_rank_new: i32,
    /// The user's rank before it — `0` if they had no entry.
    pub global_rank_previous: i32,
}

/// How one leaderboard request finished.
#[derive(Debug, Clone, PartialEq)]
pub enum LeaderboardOutcome {
    /// A find/create resolved. `None` means the backend answered
    /// successfully that no board by that name exists — a normal answer, not
    /// a failure, and worth distinguishing from [`Failed`](Self::Failed).
    Board(Option<LeaderboardInfo>),
    /// A score upload finished.
    Uploaded(ScoreUploaded),
    /// A download finished. An empty vec means the requested range held no
    /// rows, which is ordinary for an around-user request on an empty board.
    Entries(Vec<LeaderboardEntry>),
    /// The request failed. The message is actionable — a stale handle says
    /// so, rather than a bare failure.
    Failed(String),
}

/// One finished leaderboard request, matched to its caller by `request`.
#[derive(Debug, Clone, PartialEq)]
pub struct LeaderboardResult {
    /// The id the originating call returned.
    pub request: u64,
    /// How it finished.
    pub outcome: LeaderboardOutcome,
}

/// Leaderboard find/upload/download. Landed Phase 9.
///
/// **This is the engine's first asynchronous platform surface.** Every call
/// here returns a request id immediately and finishes later, through
/// [`poll`](Self::poll) — the backend's own results arrive on a callback the
/// engine can only observe while [`Platform::pump`] is running, so a
/// synchronous answer is not available to give.
///
/// **Exactly one [`LeaderboardResult`] comes back per request id**, always,
/// including for a call that was doomed the moment it was made (an unknown
/// board handle). A caller can therefore key pending state on the id and know
/// it will be cleared, rather than needing a timeout of its own.
///
/// **A board handle is session-scoped**, because the backend binding this
/// engine uses can read a handle's raw value but cannot construct one back
/// from it. Find the board by name each session; don't persist the id.
pub trait Leaderboards {
    /// Looks up a board by name. Resolves to
    /// [`LeaderboardOutcome::Board`]`(None)` if no such board exists.
    fn find(&self, name: &str) -> u64;
    /// Looks up a board by name, creating it with `sort`/`display` if it
    /// doesn't exist. Creating boards this way is a development convenience —
    /// a shipping game's boards are normally declared on the backend's own
    /// admin site, where they can also be reset and moderated.
    fn find_or_create(&self, name: &str, sort: LeaderboardSort, display: LeaderboardDisplay)
    -> u64;
    /// Uploads `score` for the local user, with an opaque `details` payload
    /// (the backend caps its length; anything past the cap is dropped rather
    /// than failing the upload).
    fn upload(&self, board: u64, method: UploadMethod, score: i32, details: &[i32]) -> u64;
    /// Downloads rows `start..=end` of `board` under `scope`. Ranks are
    /// 1-based for [`LeaderboardScope::Global`]/[`Friends`](LeaderboardScope::Friends)
    /// and relative (negative = better than the local user) for
    /// [`GlobalAroundUser`](LeaderboardScope::GlobalAroundUser).
    fn download(&self, board: u64, scope: LeaderboardScope, start: i32, end: i32) -> u64;
    /// Takes every request that has finished since the last call. A drain,
    /// not a peek — matching [`Identity::poll_persona_change`] and the
    /// engine's per-frame callback-drain pattern.
    fn poll(&self) -> Vec<LeaderboardResult>;
}

/// Who can see and join a lobby.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LobbyKind {
    /// Invite-only, and not returned by a search.
    Private,
    /// The creator's friends can find it; nobody else can.
    FriendsOnly,
    /// Anyone can find it through a search.
    Public,
    /// Joinable by anyone who knows its id, but never returned by a search —
    /// for a game that runs its own matchmaking and uses lobbies only as the
    /// meeting point.
    Invisible,
}

/// How far afield a lobby search should look. Steam works this out from the
/// data-centre regions involved, not from anything the player configures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LobbyDistance {
    /// The same region only.
    Close,
    /// Nearby regions. Steam's own default.
    Default,
    /// A wide radius — half the planet.
    Far,
    /// No distance filtering at all.
    Worldwide,
}

/// How a numeric lobby filter compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LobbyCompare {
    /// The lobby's value equals the one asked for.
    Equal,
    /// …does not equal it.
    NotEqual,
    /// …is strictly greater.
    Greater,
    /// …is greater or equal.
    GreaterOrEqual,
    /// …is strictly less.
    Less,
    /// …is less or equal.
    LessOrEqual,
}

/// What a lobby search should match. Every field is additive — a lobby has to
/// satisfy all of them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LobbyFilters {
    /// `(key, value)` pairs a lobby's own data must equal exactly.
    pub string: Vec<(String, String)>,
    /// `(key, value, how)` numeric comparisons against a lobby's own data.
    pub number: Vec<(String, i32, LobbyCompare)>,
    /// Only lobbies with at least this many free seats.
    pub slots_available: Option<u8>,
    /// How far to search.
    pub distance: Option<LobbyDistance>,
    /// Stop after this many results.
    pub max_results: Option<u64>,
}

/// A lobby, with the metadata that is readable the moment its id is known.
#[derive(Debug, Clone, PartialEq)]
pub struct LobbyInfo {
    /// The lobby's id. Unlike a [`LeaderboardInfo::id`], this one is a real
    /// identifier: it can be saved, sent to another player, and used to join
    /// later.
    pub id: u64,
    /// How many members are in it now.
    pub member_count: usize,
    /// The most it will hold, if the backend reports one.
    pub member_limit: Option<usize>,
    /// The member who owns it (the host).
    pub owner: Option<u64>,
    /// Every key/value the lobby carries — the game mode, the map, whatever
    /// the host set. This is what a lobby browser lists.
    pub data: Vec<(String, String)>,
}

/// How one lobby request finished.
#[derive(Debug, Clone, PartialEq)]
pub enum LobbyOutcome {
    /// A lobby was created, and the local user is already in it.
    Created(LobbyInfo),
    /// A lobby was joined.
    Joined(LobbyInfo),
    /// A search finished. An empty vec means nothing matched, which is
    /// ordinary rather than a failure.
    Listed(Vec<LobbyInfo>),
    /// The request failed, with an actionable reason.
    Failed(String),
}

/// One finished lobby request, matched to its caller by `request`.
#[derive(Debug, Clone, PartialEq)]
pub struct LobbyResult {
    /// The id the originating call returned.
    pub request: u64,
    /// How it finished.
    pub outcome: LobbyOutcome,
}

/// What happened to a lobby member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LobbyMemberChange {
    /// They joined.
    Entered,
    /// They left deliberately.
    Left,
    /// They dropped without leaving first.
    Disconnected,
    /// They were kicked.
    Kicked,
    /// They were kicked and banned.
    Banned,
}

/// Something that happened in a lobby the local user is in, since the last
/// [`Lobbies::poll_events`].
#[derive(Debug, Clone, PartialEq)]
pub enum LobbyEvent {
    /// A member joined or left.
    ///
    /// **Who did it is deliberately not reported.** The Steamworks binding
    /// this engine uses fills its own "who made this change" field from the
    /// *changed member's* id — so for a kick it names the person kicked, not
    /// the person kicking. Exposing a field that is wrong whenever it would
    /// be interesting is worse than not having it; see
    /// the Steam integration plan.
    MemberChanged {
        /// The lobby it happened in.
        lobby: u64,
        /// The member whose membership changed.
        user: u64,
        /// What happened to them.
        change: LobbyMemberChange,
    },
    /// A lobby's own data, or one member's data, changed. Re-read it with
    /// [`Lobbies::all_data`] or [`Lobbies::member_data`].
    DataChanged {
        /// The lobby whose data changed.
        lobby: u64,
        /// Whose data it was — equal to `lobby` when it was the lobby's own.
        member: u64,
    },
}

/// Lobby creation, search and membership. Landed Phase 6a.
///
/// **Lobbies are discovery, not transport.** A lobby is how players find each
/// other and agree on what they're about to play; it carries small key/value
/// metadata and a member list, and nothing about it decides how the game's
/// actual packets travel. A game can meet in a Steam lobby and then run its
/// session over any transport the engine has.
///
/// Asynchronous calls follow exactly the contract
/// [`Leaderboards`] established — a request id now, exactly one
/// [`LobbyResult`] later through [`poll`](Self::poll), including for a call
/// that could never have worked. The synchronous half is genuinely
/// synchronous: once a lobby's id is known, its data and member list are
/// local reads.
pub trait Lobbies {
    /// Creates a lobby and joins it. `max_members` is capped by the backend
    /// (250 on Steam); a larger number fails the request rather than being
    /// quietly reduced.
    fn create(&self, kind: LobbyKind, max_members: u32) -> u64;
    /// Joins an existing lobby by id.
    fn join(&self, lobby: u64) -> u64;
    /// Searches for lobbies matching `filters`.
    fn list(&self, filters: &LobbyFilters) -> u64;
    /// Leaves a lobby. Safe to call when not in it.
    fn leave(&self, lobby: u64);

    /// Reads one of the lobby's own data values.
    fn data(&self, lobby: u64, key: &str) -> Option<String>;
    /// Every key/value the lobby carries.
    fn all_data(&self, lobby: u64) -> Vec<(String, String)>;
    /// Sets one of the lobby's own data values. **Only the owner may do
    /// this**, and `Err` says so rather than failing silently.
    fn set_data(&self, lobby: u64, key: &str, value: &str) -> Result<(), String>;
    /// Removes one of the lobby's own data values.
    fn delete_data(&self, lobby: u64, key: &str) -> Result<(), String>;
    /// Reads one of `member`'s own data values in this lobby.
    fn member_data(&self, lobby: u64, member: u64, key: &str) -> Option<String>;
    /// Sets one of the LOCAL user's data values in this lobby — their chosen
    /// character, their ready flag. Any member may set their own.
    fn set_member_data(&self, lobby: u64, key: &str, value: &str) -> Result<(), String>;

    /// Everyone currently in the lobby.
    fn members(&self, lobby: u64) -> Vec<u64>;
    /// The lobby's owner, if it has one this session can see.
    fn owner(&self, lobby: u64) -> Option<u64>;
    /// The most members the lobby will hold.
    fn member_limit(&self, lobby: u64) -> Option<usize>;
    /// Opens or closes the lobby to new members. Owner only.
    fn set_joinable(&self, lobby: u64, joinable: bool) -> Result<(), String>;

    /// Takes every request that has finished since the last call.
    fn poll(&self) -> Vec<LobbyResult>;
    /// Takes everything that has happened in the local user's lobbies since
    /// the last call.
    fn poll_events(&self) -> Vec<LobbyEvent>;
}

/// The platform capability boundary. One accessor per capability group,
/// defaulting to `None` — a backend that hasn't grown a capability yet (or
/// never will) needs no impl for it at all, and a caller checks once, at the
/// point of use, rather than the whole engine gaining a compile-time feature
/// matrix.
pub trait Platform {
    /// Whether this backend is actually available right now — a real backend
    /// whose runtime prerequisite succeeded (a Steam client was running and
    /// `SteamAPI_Init` succeeded, for `floptle_steam::SteamPlatform`), not
    /// just "compiled in". `NullPlatform` always answers `false`.
    fn available(&self) -> bool {
        false
    }
    /// Pumps pending backend callbacks. Call once per frame, main thread
    /// only, for `floptle run`/exported/served builds — never inside the
    /// editor's own docked Play-mode viewport (see
    /// the Steam integration plan's "Where Steam activates").
    /// `NullPlatform` has nothing to pump.
    fn pump(&self) {}
    /// The [`Achievements`] surface, if this backend has one.
    fn achievements(&self) -> Option<&dyn Achievements> {
        None
    }
    /// The [`Cloud`] surface, if this backend has one.
    fn cloud(&self) -> Option<&dyn Cloud> {
        None
    }
    /// The [`Identity`] surface, if this backend has one.
    fn identity(&self) -> Option<&dyn Identity> {
        None
    }
    /// The [`Entitlements`] surface, if this backend has one.
    fn entitlements(&self) -> Option<&dyn Entitlements> {
        None
    }
    /// The [`Ugc`] surface, if this backend has one.
    fn ugc(&self) -> Option<&dyn Ugc> {
        None
    }
    /// The [`Overlay`] surface, if this backend has one.
    fn overlay(&self) -> Option<&dyn Overlay> {
        None
    }
    /// The [`Input`] surface, if this backend has one.
    fn input(&self) -> Option<&dyn Input> {
        None
    }
    /// The [`Social`] surface, if this backend has one.
    fn social(&self) -> Option<&dyn Social> {
        None
    }
    /// The [`Leaderboards`] surface, if this backend has one.
    fn leaderboards(&self) -> Option<&dyn Leaderboards> {
        None
    }
    /// The [`Lobbies`] surface, if this backend has one.
    fn lobbies(&self) -> Option<&dyn Lobbies> {
        None
    }
}

/// The always-available, no-external-dependency default: every capability
/// accessor answers `None`. This is what a headless test, an in-editor Play
/// session, or a project with no `steam` setting runs against.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullPlatform;

impl Platform for NullPlatform {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_platform_answers_none_for_every_capability() {
        let p = NullPlatform;
        assert!(!p.available());
        assert!(p.achievements().is_none());
        assert!(p.cloud().is_none());
        assert!(p.identity().is_none());
        assert!(p.entitlements().is_none());
        assert!(p.ugc().is_none());
        assert!(p.overlay().is_none());
        assert!(p.input().is_none());
        assert!(p.social().is_none());
        assert!(p.leaderboards().is_none());
        assert!(p.lobbies().is_none());
    }

    /// `Platform` must be usable as `&dyn Platform` (call sites hold a boxed
    /// or referenced backend, never a concrete type) — a bound or method that
    /// broke object-safety would fail here, not at some downstream call site.
    #[test]
    fn platform_is_object_safe() {
        let p = NullPlatform;
        let dyn_p: &dyn Platform = &p;
        dyn_p.pump();
        assert!(!dyn_p.available());
        assert!(dyn_p.achievements().is_none());
    }
}
