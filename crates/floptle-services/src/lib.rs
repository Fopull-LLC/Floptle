//! # floptle-services — the platform capability boundary
//!
//! Phase 0 of `docs/steam-integration-proposal.md`. [`Platform`] is the one
//! trait every downstream crate depends on; a platform SDK's own types
//! (Steamworks today, in [`floptle_steam`](../floptle_steam/index.html), gated
//! behind its `steam` feature) never reach `floptle-script`,
//! `floptle-runtime`, or `floptle-editor` directly.
//!
//! Shape mirrors `floptle_net::Transport`: one small trait per concern,
//! composed, with an always-available no-dependency default —
//! [`NullPlatform`] here, `MemoryTransport` there. Each sub-trait
//! ([`Achievements`], [`Cloud`], [`Identity`], [`Entitlements`], [`Ugc`],
//! [`Overlay`], [`Input`], [`Social`]) starts empty on purpose: its methods
//! land with the phase that actually needs them (Achievements in Phase 2,
//! Overlay in Phase 3, and so on), so this crate's job is the boundary shape,
//! not a guessed-ahead capability surface.
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
/// `docs/steam-integration-proposal.md`. **So is progress-indicator
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
/// `docs/steam-integration-proposal.md`.
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
    /// callback-drain pattern (`docs/steam-integration-proposal.md`).
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

/// Friends, presence and invites surface. Empty until Phase 5.
pub trait Social {}

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
    /// `docs/steam-integration-proposal.md`'s "Where Steam activates").
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
