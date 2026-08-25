//! `steam.*` — the game-runtime Lua surface over `floptle_services::Platform`.
//!
//! **Nil, not an error, is what "not on Steam right now" looks like.** Every
//! getter below reads through `Platform::identity()`, which is `None` under
//! `NullPlatform` — an authoring session, the editor's own docked Play-mode
//! viewport, or a `floptle run`/exported build with no Steam client running
//! all answer this way, and it is an ordinary branch a script takes with
//! `steam.available()`, never a raised error.
//!
//! **`localUserId` is a string.** A SteamID64 routinely exceeds 2^53, past
//! where an `f64` (every Lua number) stops representing an integer exactly —
//! returning one as a Lua number would silently round it.
//!
//! **Avatars are not exposed here yet.** `Identity::avatar_small/medium/large`
//! exist at the Rust level (raw RGBA8 bytes), but turning them into something
//! a script can actually draw needs a runtime texture-from-bytes primitive
//! that doesn't exist anywhere in the engine yet — that's its own piece of
//! infrastructure, not a Steam-specific gap, and is deliberately left for a
//! follow-up rather than shipped as bytes a script has no way to use.
//!
//! **Achievement/stat writes never hit the network directly.** Every unlock,
//! clear or stat write marks the backend dirty; an automatic batch (or an
//! explicit `steam.flushStats()`) sends everything pending in one call,
//! reconciling with the backend's own async confirmation rather than trusting
//! the synchronous call's own `Ok` — see `floptle_steam::SteamPlatform`'s
//! `pump`/`flush`. **Average-rate stats and progress-indicator notifications
//! are out of scope** — the Steamworks binding this engine uses doesn't wrap
//! either at all (`docs/steam-integration-proposal.md`).
//!
//! **Cloud saves (`steam.cloud*`) have no conflict policy of their own** —
//! `steam.cloudFileTimestamp` is the primitive a script compares against its
//! own local save's modification time to decide what "newer" means for
//! itself. Data is a binary-safe Lua string in and out, same as
//! `ed.readBytes` elsewhere in this engine.
//!
//! **Friend ids (`steam.friends`, `steam.friendRichPresence`) are strings**,
//! same reasoning as `localUserId`. `steam.friends()` returns the CALLER's
//! friend list; `steam.friendRichPresence(id, key)` reads one of THAT
//! friend's own rich-presence values, set by their own game calling
//! `steam.setRichPresence` — different from reading your own.
//!
//! **Leaderboards are asynchronous, and their callback always runs exactly
//! once.** `steam.findLeaderboard`, `findOrCreateLeaderboard`, `uploadScore`
//! and `downloadScores` each hand their answer to a callback on a LATER frame
//! — never inline — and they do so in every session, including one with no
//! Steam at all, where the callback gets `(nil, "Steam isn't available…")`.
//! That is deliberate: the alternative is a call that answers `(false, err)`
//! immediately when there's no backend and through a callback when there is,
//! which gives a game two failure paths where the second is the one nobody
//! writes. A board handle is a STRING (`localUserId`'s reasoning again) and
//! lasts only for the session that resolved it — Steam can read a handle's raw
//! value but cannot construct one back from it, so there is nothing useful to
//! persist.
//!
//! Installed unconditionally (`ScriptHost::new()`), same as every other
//! `install_*_api` — the `steam` global always exists so `steam.available()`
//! is always safe to call. What varies is only what `platform` currently
//! points at: `NullPlatform` by default, swapped for a real
//! `floptle_steam::SteamPlatform` by `ScriptHost::set_platform` when (and
//! only when) the caller has decided this session IS the game — see
//! `docs/steam-integration-proposal.md`'s "Where Steam activates".

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use mlua::{Lua, Table, Value};

use crate::{LogLevel, ScriptLog};
use floptle_services::{
    Achievements, Cloud, LeaderboardDisplay, LeaderboardInfo, LeaderboardOutcome, LeaderboardScope,
    LeaderboardSort, Platform, Social, UploadMethod,
};

/// The message every `steam.*` call answers with when no backend is present.
const NO_STEAM: &str = "Steam isn't available in this session";

/// The platform backend, swappable after `ScriptHost::new()` via
/// `set_platform` — every Lua closure below holds a clone of this same cell,
/// so a later swap is visible to all of them without reinstalling anything.
pub(crate) type SharedPlatform = Rc<RefCell<Rc<dyn Platform>>>;

/// State private to the `steam` API: the registered persona-change callback,
/// and the leaderboard callbacks still waiting on a backend result — all
/// drained once per frame like every other event queue here.
#[derive(Default)]
pub(crate) struct SteamState {
    persona_changed_cb: Option<mlua::Function>,
    /// Leaderboard callbacks keyed by the request id the backend handed back.
    ///
    /// No generation counter guards this, unlike `HttpState` — a backend never
    /// reuses a request id for the life of the process, so a result from a
    /// finished Play session finds nothing here and is dropped, rather than
    /// landing on a fresh session's callback. `cancel_all` is what makes that
    /// true from this side.
    lb_pending: HashMap<u64, mlua::Function>,
    /// Calls that failed before reaching a backend at all — no Steam in this
    /// session, or a malformed board handle. Held here and fired by the next
    /// [`drain`] so that **a leaderboard callback runs exactly once, on a
    /// later frame, in every session**: a script has one place to handle
    /// failure instead of one for "Steam said no" and another for "there was
    /// no Steam to ask".
    lb_failed: Vec<(mlua::Function, String)>,
}

impl SteamState {
    /// Stop / scene load / a platform swap: forget every callback.
    ///
    /// Same rule as `http.*` — a callback registered last session closes over
    /// nodes that no longer exist, so firing it into a fresh Play is how one
    /// run inherits the previous one's leaderboard traffic. The persona
    /// callback goes too, for exactly the same reason.
    pub(crate) fn cancel_all(&mut self) {
        self.lb_pending.clear();
        self.lb_failed.clear();
        self.persona_changed_cb = None;
    }

    /// How many leaderboard requests are still waiting
    /// (`steam.leaderboardsInFlight()`).
    pub(crate) fn in_flight(&self) -> usize {
        self.lb_pending.len() + self.lb_failed.len()
    }
}

fn log(logs: &Rc<RefCell<Vec<ScriptLog>>>, level: LogLevel, msg: String) {
    logs.borrow_mut().push(ScriptLog { level, msg, source: None });
}

/// Runs `f` against the current backend's `Achievements` surface, or answers
/// a plain "not available" error when there is none — every achievement/stat
/// WRITE call here goes through this, so `steam.unlockAchievement(...)`
/// against `NullPlatform` (no Steam) answers `(false, "...")`, not a crash on
/// calling a method that doesn't exist.
fn achievements_call(
    platform: &SharedPlatform,
    f: impl FnOnce(&dyn Achievements) -> Result<(), String>,
) -> Result<(), String> {
    match platform.borrow().achievements() {
        Some(a) => f(a),
        None => Err("Steam isn't available in this session".into()),
    }
}

/// `Result<(), String>` as the `(ok, err)` pair every write call here
/// returns to Lua — `err` is `nil` on success, never an empty string.
fn result_tuple(lua: &Lua, r: Result<(), String>) -> mlua::Result<(bool, Value)> {
    match r {
        Ok(()) => Ok((true, Value::Nil)),
        Err(msg) => Ok((false, Value::String(lua.create_string(msg)?))),
    }
}

/// Runs `f` against the current backend's `Cloud` surface, or answers a
/// plain "not available" error when there is none — same shape as
/// `achievements_call`, generic over the read/write return type.
fn cloud_call<T>(
    platform: &SharedPlatform,
    f: impl FnOnce(&dyn Cloud) -> Result<T, String>,
) -> Result<T, String> {
    match platform.borrow().cloud() {
        Some(c) => f(c),
        None => Err("Steam isn't available in this session".into()),
    }
}

/// Runs `f` against the current backend's `Social` surface, or answers a
/// plain "not available" error when there is none — same shape as
/// `achievements_call`/`cloud_call`.
fn social_call<T>(
    platform: &SharedPlatform,
    f: impl FnOnce(&dyn Social) -> Result<T, String>,
) -> Result<T, String> {
    match platform.borrow().social() {
        Some(s) => f(s),
        None => Err("Steam isn't available in this session".into()),
    }
}

/// Keys `steam.findOrCreateLeaderboard`'s options table reads.
pub(crate) const CREATE_KEYS: &[&str] = &["sort", "display"];
/// Keys `steam.uploadScore`'s options table reads.
pub(crate) const UPLOAD_KEYS: &[&str] = &["method", "details"];
/// Keys `steam.downloadScores`' options table reads.
pub(crate) const DOWNLOAD_KEYS: &[&str] = &["scope", "start", "count"];

/// The most rows one `steam.downloadScores` will ask for. Steam pages long
/// boards and so should a caller; the cap also keeps `start + count` inside an
/// `i32` for any `start` a rank can legitimately be.
const MAX_ROWS: i32 = 10_000;

const SORTS: &[&str] = &["ascending", "descending"];
const DISPLAYS: &[&str] = &["numeric", "seconds", "milliseconds"];
const SCOPES: &[&str] = &["global", "aroundUser", "friends"];
const METHODS: &[&str] = &["keepBest", "forceUpdate"];

fn parse_sort(s: &str) -> Option<LeaderboardSort> {
    match s {
        "ascending" => Some(LeaderboardSort::Ascending),
        "descending" => Some(LeaderboardSort::Descending),
        _ => None,
    }
}

fn parse_display(s: &str) -> Option<LeaderboardDisplay> {
    match s {
        "numeric" => Some(LeaderboardDisplay::Numeric),
        "seconds" => Some(LeaderboardDisplay::TimeSeconds),
        "milliseconds" => Some(LeaderboardDisplay::TimeMilliseconds),
        _ => None,
    }
}

fn parse_scope(s: &str) -> Option<LeaderboardScope> {
    match s {
        "global" => Some(LeaderboardScope::Global),
        "aroundUser" => Some(LeaderboardScope::GlobalAroundUser),
        "friends" => Some(LeaderboardScope::Friends),
        _ => None,
    }
}

fn parse_method(s: &str) -> Option<UploadMethod> {
    match s {
        "keepBest" => Some(UploadMethod::KeepBest),
        "forceUpdate" => Some(UploadMethod::ForceUpdate),
        _ => None,
    }
}

fn sort_str(s: LeaderboardSort) -> &'static str {
    match s {
        LeaderboardSort::Ascending => "ascending",
        LeaderboardSort::Descending => "descending",
    }
}

fn display_str(d: LeaderboardDisplay) -> &'static str {
    match d {
        LeaderboardDisplay::Numeric => "numeric",
        LeaderboardDisplay::TimeSeconds => "seconds",
        LeaderboardDisplay::TimeMilliseconds => "milliseconds",
    }
}

/// Read an enumerated string option, falling back to `default` when absent.
fn opt_enum<T>(
    t: &Option<Table>,
    call: &str,
    key: &str,
    accepted: &[&str],
    parse: impl Fn(&str) -> Option<T>,
    default: T,
) -> mlua::Result<T> {
    let Some(t) = t else { return Ok(default) };
    match t.get::<Value>(key)? {
        Value::Nil => Ok(default),
        Value::String(s) => {
            crate::opts::parse_enum(call, key, &s.to_str()?, accepted, parse)
        }
        other => Err(mlua::Error::RuntimeError(format!(
            "{call}: `{key}` takes one of {} as a string, got {}",
            accepted.join(", "),
            other.type_name()
        ))),
    }
}

/// Read an integer option, falling back to `default` when absent.
///
/// A value too big for an `i32` is REFUSED, not truncated. `as` would turn
/// `start = 4294967296` into `0` and download the wrong rows while reporting
/// success — the silent-wrong-answer shape `crate::opts` exists to stop.
fn opt_i32(t: &Option<Table>, call: &str, key: &str, default: i32) -> mlua::Result<i32> {
    let Some(t) = t else { return Ok(default) };
    let too_big = |v: i64| {
        mlua::Error::RuntimeError(format!(
            "{call}: `{key} = {v}` is outside the range a leaderboard rank can be \
             ({} to {})",
            i32::MIN,
            i32::MAX
        ))
    };
    match t.get::<Value>(key)? {
        Value::Nil => Ok(default),
        Value::Integer(i) => i32::try_from(i).map_err(|_| too_big(i)),
        Value::Number(n) if n.fract() == 0.0 && (i32::MIN as f64..=i32::MAX as f64).contains(&n) => {
            Ok(n as i32)
        }
        Value::Number(n) if n.fract() == 0.0 => Err(too_big(n as i64)),
        Value::Number(n) => Err(mlua::Error::RuntimeError(format!(
            "{call}: `{key} = {n}` is not a whole number"
        ))),
        other => Err(mlua::Error::RuntimeError(format!(
            "{call}: `{key}` takes a whole number, got {}",
            other.type_name()
        ))),
    }
}

/// A board handle as a script carries it: a STRING, same reasoning as
/// `localUserId` — a handle is a full `u64` and a Lua number would round it.
fn parse_board_id(id: &str) -> Option<u64> {
    id.parse::<u64>().ok()
}

/// Split a trailing-callback call's `(maybe-opts, maybe-callback)` tail.
///
/// Both `steam.uploadScore(id, 10, cb)` and
/// `steam.uploadScore(id, 10, { … }, cb)` are ordinary things to write, and a
/// call that silently ignored the callback in the first form would be exactly
/// the silent-acceptance failure `crate::opts` exists to stop.
fn opts_and_callback(
    call: &str,
    a: Value,
    b: Value,
) -> mlua::Result<(Option<Table>, mlua::Function)> {
    match (a, b) {
        (Value::Function(f), Value::Nil) => Ok((None, f)),
        (Value::Table(t), Value::Function(f)) => Ok((Some(t), f)),
        (Value::Table(_), other) => Err(mlua::Error::RuntimeError(format!(
            "{call}: an options table must be followed by the callback function, got {}",
            other.type_name()
        ))),
        (other, _) => Err(mlua::Error::RuntimeError(format!(
            "{call}: expected a callback function (or an options table then the callback), got {}",
            other.type_name()
        ))),
    }
}

/// Queue `cb` against `request`, or — when there is no backend to have issued
/// one — queue the failure for the next drain. Either way the callback fires
/// exactly once, later, never inline.
fn queue(state: &Rc<RefCell<SteamState>>, request: Option<u64>, cb: mlua::Function, why: &str) {
    let mut s = state.borrow_mut();
    match request {
        Some(id) => {
            s.lb_pending.insert(id, cb);
        }
        None => s.lb_failed.push((cb, why.to_string())),
    }
}

/// `LeaderboardInfo` as the Lua table a script reads.
fn board_table(lua: &Lua, info: &LeaderboardInfo) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("id", lua.create_string(info.id.to_string())?)?;
    t.set("name", lua.create_string(&info.name)?)?;
    t.set("entryCount", info.entry_count)?;
    t.set("sort", info.sort.map(sort_str))?;
    t.set("display", info.display.map(display_str))?;
    Ok(t)
}

/// Install the `steam` global table.
pub(crate) fn install_steam_api(
    lua: &Lua,
    platform: SharedPlatform,
    state: Rc<RefCell<SteamState>>,
) -> mlua::Result<()> {
    let t = lua.create_table()?;

    let p = platform.clone();
    t.set("available", lua.create_function(move |_, ()| Ok(p.borrow().available()))?)?;

    let p = platform.clone();
    t.set(
        "localUserId",
        lua.create_function(move |lua, ()| match p.borrow().identity() {
            Some(id) => Ok(Value::String(lua.create_string(id.local_user_id().to_string())?)),
            None => Ok(Value::Nil),
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "personaName",
        lua.create_function(move |lua, ()| match p.borrow().identity() {
            Some(id) => Ok(Value::String(lua.create_string(id.persona_name())?)),
            None => Ok(Value::Nil),
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "buildId",
        lua.create_function(move |_, ()| {
            Ok(p.borrow().identity().map(|id| id.build_id()))
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "installDir",
        lua.create_function(move |lua, ()| match p.borrow().identity() {
            Some(id) => Ok(Value::String(lua.create_string(id.install_dir())?)),
            None => Ok(Value::Nil),
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "betaName",
        lua.create_function(move |lua, ()| match p.borrow().identity().and_then(|id| id.beta_name()) {
            Some(name) => Ok(Value::String(lua.create_string(name)?)),
            None => Ok(Value::Nil),
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "isFamilyShared",
        lua.create_function(move |_, ()| Ok(p.borrow().identity().map(|id| id.is_family_shared())))?,
    )?;

    let p = platform.clone();
    t.set(
        "isCybercafe",
        lua.create_function(move |_, ()| Ok(p.borrow().identity().map(|id| id.is_cybercafe())))?,
    )?;

    let p = platform.clone();
    t.set(
        "uiLanguage",
        lua.create_function(move |lua, ()| match p.borrow().identity() {
            Some(id) => Ok(Value::String(lua.create_string(id.ui_language())?)),
            None => Ok(Value::Nil),
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "isSteamDeck",
        lua.create_function(move |_, ()| Ok(p.borrow().identity().map(|id| id.is_steam_deck())))?,
    )?;

    let p = platform.clone();
    t.set(
        "isBigPictureMode",
        lua.create_function(move |_, ()| Ok(p.borrow().identity().map(|id| id.is_big_picture_mode())))?,
    )?;

    let p = platform.clone();
    t.set("statsReady", lua.create_function(move |_, ()| Ok(p.borrow().achievements().is_some_and(|a| a.stats_ready())))?)?;

    let p = platform.clone();
    t.set(
        "achievementUnlocked",
        lua.create_function(move |_, id: String| {
            Ok(p.borrow().achievements().and_then(|a| a.achievement_unlocked(&id)))
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "unlockAchievement",
        lua.create_function(move |lua, id: String| {
            result_tuple(lua, achievements_call(&p, |a| a.unlock_achievement(&id)))
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "clearAchievement",
        lua.create_function(move |lua, id: String| {
            result_tuple(lua, achievements_call(&p, |a| a.clear_achievement(&id)))
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "achievementGlobalPercent",
        lua.create_function(move |_, id: String| {
            Ok(p.borrow().achievements().and_then(|a| a.achievement_global_percent(&id)))
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "achievementName",
        lua.create_function(move |lua, id: String| {
            match p.borrow().achievements().and_then(|a| a.achievement_name(&id)) {
                Some(name) => Ok(Value::String(lua.create_string(name)?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "achievementDescription",
        lua.create_function(move |lua, id: String| {
            match p.borrow().achievements().and_then(|a| a.achievement_description(&id)) {
                Some(desc) => Ok(Value::String(lua.create_string(desc)?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "statInt",
        lua.create_function(move |_, name: String| {
            Ok(p.borrow().achievements().and_then(|a| a.stat_int(&name)))
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "setStatInt",
        lua.create_function(move |lua, (name, value): (String, i32)| {
            result_tuple(lua, achievements_call(&p, |a| a.set_stat_int(&name, value)))
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "statFloat",
        lua.create_function(move |_, name: String| {
            Ok(p.borrow().achievements().and_then(|a| a.stat_float(&name)))
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "setStatFloat",
        lua.create_function(move |lua, (name, value): (String, f32)| {
            result_tuple(lua, achievements_call(&p, |a| a.set_stat_float(&name, value)))
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "flushStats",
        lua.create_function(move |_, ()| {
            if let Some(a) = p.borrow().achievements() {
                a.flush();
            }
            Ok(())
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "resetAllStats",
        lua.create_function(move |lua, achievements_too: bool| {
            result_tuple(lua, achievements_call(&p, |a| a.reset_all_stats(achievements_too)))
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "cloudEnabled",
        lua.create_function(move |_, ()| Ok(p.borrow().cloud().map(|c| c.is_enabled_for_app())))?,
    )?;

    let p = platform.clone();
    t.set(
        "setCloudEnabled",
        lua.create_function(move |lua, enabled: bool| {
            result_tuple(
                lua,
                cloud_call(&p, |c| {
                    c.set_enabled_for_app(enabled);
                    Ok(())
                }),
            )
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "cloudEnabledForAccount",
        lua.create_function(move |_, ()| Ok(p.borrow().cloud().map(|c| c.is_enabled_for_account())))?,
    )?;

    let p = platform.clone();
    t.set(
        "cloudFiles",
        lua.create_function(move |lua, ()| match p.borrow().cloud() {
            Some(c) => {
                let t = lua.create_table()?;
                for (i, (name, size)) in c.files().into_iter().enumerate() {
                    let row = lua.create_table()?;
                    row.set("name", name)?;
                    row.set("size", size)?;
                    t.set(i + 1, row)?;
                }
                Ok(Value::Table(t))
            }
            None => Ok(Value::Nil),
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "cloudFileExists",
        lua.create_function(move |_, name: String| {
            Ok(p.borrow().cloud().map(|c| c.file_exists(&name)))
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "cloudFileTimestamp",
        lua.create_function(move |_, name: String| {
            Ok(p.borrow().cloud().and_then(|c| c.file_timestamp(&name)))
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "cloudDelete",
        lua.create_function(move |lua, name: String| {
            result_tuple(lua, cloud_call(&p, |c| c.delete_file(&name)))
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "cloudForget",
        lua.create_function(move |lua, name: String| {
            result_tuple(lua, cloud_call(&p, |c| c.forget_file(&name)))
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "cloudRead",
        lua.create_function(move |lua, name: String| {
            match cloud_call(&p, |c| c.read_file(&name)) {
                Ok(bytes) => Ok((Value::String(lua.create_string(&bytes)?), Value::Nil)),
                Err(msg) => Ok((Value::Nil, Value::String(lua.create_string(msg)?))),
            }
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "cloudWrite",
        lua.create_function(move |lua, (name, data): (String, mlua::String)| {
            result_tuple(lua, cloud_call(&p, |c| c.write_file(&name, &data.as_bytes())))
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "setRichPresence",
        lua.create_function(move |lua, (key, value): (String, String)| {
            result_tuple(lua, social_call(&p, |s| s.set_rich_presence(&key, &value)))
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "clearRichPresence",
        lua.create_function(move |_, ()| {
            if let Some(s) = p.borrow().social() {
                s.clear_rich_presence();
            }
            Ok(())
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "friends",
        lua.create_function(move |lua, ()| match p.borrow().social() {
            Some(s) => {
                let t = lua.create_table()?;
                for (i, f) in s.friends().into_iter().enumerate() {
                    let row = lua.create_table()?;
                    row.set("id", lua.create_string(f.id.to_string())?)?;
                    row.set("name", f.name)?;
                    row.set("state", f.state)?;
                    row.set("playingThisGame", f.playing_this_game)?;
                    t.set(i + 1, row)?;
                }
                Ok(Value::Table(t))
            }
            None => Ok(Value::Nil),
        })?,
    )?;

    let p = platform.clone();
    t.set(
        "friendRichPresence",
        lua.create_function(move |lua, (friend_id, key): (String, String)| {
            let Ok(friend_id) = friend_id.parse::<u64>() else {
                return Ok(Value::Nil);
            };
            match p.borrow().social().and_then(|s| s.friend_rich_presence(friend_id, &key)) {
                Some(v) => Ok(Value::String(lua.create_string(v)?)),
                None => Ok(Value::Nil),
            }
        })?,
    )?;

    let (p, st) = (platform.clone(), state.clone());
    t.set(
        "findLeaderboard",
        lua.create_function(move |_, (name, cb): (String, mlua::Function)| {
            let request = p.borrow().leaderboards().map(|l| l.find(&name));
            queue(&st, request, cb, NO_STEAM);
            Ok(())
        })?,
    )?;

    let (p, st) = (platform.clone(), state.clone());
    t.set(
        "findOrCreateLeaderboard",
        lua.create_function(move |_, (name, a, b): (String, Value, Value)| {
            let call = "steam.findOrCreateLeaderboard";
            let (opts, cb) = opts_and_callback(call, a, b)?;
            if let Some(o) = &opts {
                crate::opts::check_keys(o, CREATE_KEYS, call)?;
            }
            let sort =
                opt_enum(&opts, call, "sort", SORTS, parse_sort, LeaderboardSort::Descending)?;
            let display = opt_enum(
                &opts,
                call,
                "display",
                DISPLAYS,
                parse_display,
                LeaderboardDisplay::Numeric,
            )?;
            let request =
                p.borrow().leaderboards().map(|l| l.find_or_create(&name, sort, display));
            queue(&st, request, cb, NO_STEAM);
            Ok(())
        })?,
    )?;

    let (p, st) = (platform.clone(), state.clone());
    t.set(
        "uploadScore",
        lua.create_function(move |_, (board, score, a, b): (String, i32, Value, Value)| {
            let call = "steam.uploadScore";
            let (opts, cb) = opts_and_callback(call, a, b)?;
            if let Some(o) = &opts {
                crate::opts::check_keys(o, UPLOAD_KEYS, call)?;
            }
            let method =
                opt_enum(&opts, call, "method", METHODS, parse_method, UploadMethod::KeepBest)?;
            let details: Vec<i32> = match opts.as_ref().map(|o| o.get::<Value>("details")) {
                None | Some(Ok(Value::Nil)) => Vec::new(),
                Some(Ok(Value::Table(d))) => {
                    d.sequence_values::<i32>().collect::<mlua::Result<Vec<i32>>>().map_err(
                        |e| {
                            mlua::Error::RuntimeError(format!(
                                "{call}: `details` takes a list of whole numbers — {e}"
                            ))
                        },
                    )?
                }
                Some(Ok(other)) => {
                    return Err(mlua::Error::RuntimeError(format!(
                        "{call}: `details` takes a list of whole numbers, got {}",
                        other.type_name()
                    )));
                }
                Some(Err(e)) => return Err(e),
            };
            let Some(id) = parse_board_id(&board) else {
                queue(&st, None, cb, &bad_handle(call, &board));
                return Ok(());
            };
            let request =
                p.borrow().leaderboards().map(|l| l.upload(id, method, score, &details));
            queue(&st, request, cb, NO_STEAM);
            Ok(())
        })?,
    )?;

    let (p, st) = (platform.clone(), state.clone());
    t.set(
        "downloadScores",
        lua.create_function(move |_, (board, a, b): (String, Value, Value)| {
            let call = "steam.downloadScores";
            let (opts, cb) = opts_and_callback(call, a, b)?;
            if let Some(o) = &opts {
                crate::opts::check_keys(o, DOWNLOAD_KEYS, call)?;
            }
            let scope =
                opt_enum(&opts, call, "scope", SCOPES, parse_scope, LeaderboardScope::Global)?;
            let start = opt_i32(&opts, call, "start", 1)?;
            let count = opt_i32(&opts, call, "count", 10)?;
            // An upper bound as well as a lower one: `start + count` has to
            // stay inside an i32, and a request for millions of rows is a
            // mistake rather than an intention worth honouring.
            if !(1..=MAX_ROWS).contains(&count) {
                return Err(mlua::Error::RuntimeError(format!(
                    "{call}: `count = {count}` is outside 1 – {MAX_ROWS} — page through \
                     a long board with `start` instead of asking for it all at once"
                )));
            }
            let Some(id) = parse_board_id(&board) else {
                queue(&st, None, cb, &bad_handle(call, &board));
                return Ok(());
            };
            // `count`, not an end rank, because an end rank makes the caller
            // do the off-by-one — and for an around-user request, where
            // `start` is negative, that arithmetic is where it goes wrong.
            // Saturating because `start` alone can still be i32::MAX even
            // with `count` bounded, and an overflow here would panic the
            // whole script host over a bad number in one options table.
            let end = start.saturating_add(count - 1);
            let request = p.borrow().leaderboards().map(|l| l.download(id, scope, start, end));
            queue(&st, request, cb, NO_STEAM);
            Ok(())
        })?,
    )?;

    let st = state.clone();
    t.set(
        "leaderboardsInFlight",
        lua.create_function(move |_, ()| Ok(st.borrow().in_flight()))?,
    )?;

    t.set(
        "onPersonaChanged",
        lua.create_function(move |_, f: mlua::Function| {
            state.borrow_mut().persona_changed_cb = Some(f);
            Ok(())
        })?,
    )?;

    lua.globals().set("steam", t)?;
    Ok(())
}

/// The message for a board handle that isn't even a number — distinct from
/// the backend's own "not from this session", because the cause is different
/// and so is the fix.
fn bad_handle(call: &str, board: &str) -> String {
    format!(
        "{call}: \"{board}\" isn't a leaderboard handle — pass the `id` from a \
         steam.findLeaderboard callback's board table, as a string"
    )
}

/// Once per frame: pumps the backend's callbacks, fires
/// `steam.onPersonaChanged` if the local user's persona changed, and delivers
/// every leaderboard request that finished.
///
/// Called from the host's FRAME pass, never the tick pass — same rule as
/// `http.*`: a backend reply arrives when it arrives, so a rollback replay
/// must never see one.
pub(crate) fn drain(
    lua: &Lua,
    platform: &SharedPlatform,
    state: &Rc<RefCell<SteamState>>,
    logs: &Rc<RefCell<Vec<ScriptLog>>>,
) {
    // `pump` is what runs the backend's own callbacks, so leaderboard results
    // land DURING this borrow and are waiting by the time it is released.
    let (changed, results) = {
        let backend = platform.borrow();
        backend.pump();
        (
            backend.identity().map(|id| id.poll_persona_change()).unwrap_or(false),
            backend.leaderboards().map(|l| l.poll()).unwrap_or_default(),
        )
    };

    // Match results to callbacks with the state borrow held, and CALL with it
    // released — a callback that starts another request re-borrows the state.
    let ready: Vec<(mlua::Function, LeaderboardOutcome)> = {
        let mut s = state.borrow_mut();
        let mut out: Vec<_> = s
            .lb_failed
            .drain(..)
            .map(|(cb, why)| (cb, LeaderboardOutcome::Failed(why)))
            .collect();
        for r in results {
            // A result whose callback is gone was started by a Play session
            // that has since stopped. Dropping it is the point.
            if let Some(cb) = s.lb_pending.remove(&r.request) {
                out.push((cb, r.outcome));
            }
        }
        out
    };

    let persona_cb = changed.then(|| state.borrow().persona_changed_cb.clone()).flatten();
    if let Some(cb) = persona_cb
        && let Err(e) = cb.call::<()>(())
    {
        log(logs, LogLevel::Error, format!("steam.onPersonaChanged callback: {e}"));
    }

    for (cb, outcome) in ready {
        if let Err(e) = deliver(lua, &cb, outcome) {
            log(logs, LogLevel::Error, format!("steam leaderboard callback: {e}"));
        }
    }
}

/// Call one leaderboard callback with `(value, err)` — the same two-value
/// shape as `steam.cloudRead`, so every asynchronous answer in this API reads
/// the same way: a value and no error, or nil and a message.
fn deliver(lua: &Lua, cb: &mlua::Function, outcome: LeaderboardOutcome) -> mlua::Result<()> {
    match outcome {
        LeaderboardOutcome::Failed(why) => {
            cb.call::<()>((Value::Nil, lua.create_string(why)?))
        }
        // A board that simply doesn't exist is nil with NO error: the backend
        // answered the question successfully, and "no such board" is a normal
        // answer a script branches on rather than an failure it reports.
        LeaderboardOutcome::Board(None) => cb.call::<()>((Value::Nil, Value::Nil)),
        LeaderboardOutcome::Board(Some(info)) => {
            cb.call::<()>((board_table(lua, &info)?, Value::Nil))
        }
        LeaderboardOutcome::Uploaded(u) => {
            let t = lua.create_table()?;
            t.set("score", u.score)?;
            t.set("changed", u.changed)?;
            t.set("rank", u.global_rank_new)?;
            t.set("previousRank", u.global_rank_previous)?;
            cb.call::<()>((t, Value::Nil))
        }
        LeaderboardOutcome::Entries(entries) => {
            let list = lua.create_table()?;
            for (i, e) in entries.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("userId", lua.create_string(e.user_id.to_string())?)?;
                row.set("rank", e.global_rank)?;
                row.set("score", e.score)?;
                let details = lua.create_table()?;
                for (j, d) in e.details.into_iter().enumerate() {
                    details.set(j + 1, d)?;
                }
                row.set("details", details)?;
                list.set(i + 1, row)?;
            }
            cb.call::<()>((list, Value::Nil))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_services::{
        LeaderboardEntry, LeaderboardResult, Leaderboards, NullPlatform, ScoreUploaded,
    };
    use std::cell::Cell;

    struct Fixture {
        lua: Lua,
        platform: SharedPlatform,
        state: Rc<RefCell<SteamState>>,
        logs: Rc<RefCell<Vec<ScriptLog>>>,
    }

    fn fresh() -> Fixture {
        let lua = Lua::new();
        let platform: SharedPlatform = Rc::new(RefCell::new(Rc::new(NullPlatform)));
        let state = Rc::new(RefCell::new(SteamState::default()));
        let logs = Rc::new(RefCell::new(Vec::new()));
        install_steam_api(&lua, platform.clone(), state.clone()).unwrap();
        Fixture { lua, platform, state, logs }
    }

    /// A backend that records what was asked of it and answers whatever the
    /// test queues — the leaderboard path is entirely asynchronous, so
    /// `NullPlatform` can only ever exercise its "no Steam" branch.
    #[derive(Default)]
    struct FakeBoards {
        asked: RefCell<Vec<String>>,
        ready: RefCell<Vec<LeaderboardResult>>,
        next: Cell<u64>,
    }

    impl FakeBoards {
        fn request(&self, what: String) -> u64 {
            self.asked.borrow_mut().push(what);
            let id = self.next.get() + 1;
            self.next.set(id);
            id
        }
        /// Queue the answer to request `id`.
        fn answer(&self, request: u64, outcome: LeaderboardOutcome) {
            self.ready.borrow_mut().push(LeaderboardResult { request, outcome });
        }
    }

    impl Leaderboards for FakeBoards {
        fn find(&self, name: &str) -> u64 {
            self.request(format!("find {name}"))
        }
        fn find_or_create(
            &self,
            name: &str,
            sort: LeaderboardSort,
            display: LeaderboardDisplay,
        ) -> u64 {
            self.request(format!("create {name} {sort:?} {display:?}"))
        }
        fn upload(&self, board: u64, method: UploadMethod, score: i32, details: &[i32]) -> u64 {
            self.request(format!("upload {board} {method:?} {score} {details:?}"))
        }
        fn download(&self, board: u64, scope: LeaderboardScope, start: i32, end: i32) -> u64 {
            self.request(format!("download {board} {scope:?} {start}..{end}"))
        }
        fn poll(&self) -> Vec<LeaderboardResult> {
            std::mem::take(&mut self.ready.borrow_mut())
        }
    }

    struct FakePlatform(Rc<FakeBoards>);

    impl Platform for FakePlatform {
        fn available(&self) -> bool {
            true
        }
        fn leaderboards(&self) -> Option<&dyn Leaderboards> {
            Some(&*self.0)
        }
    }

    /// A fixture whose backend answers leaderboard calls.
    fn with_boards() -> (Fixture, Rc<FakeBoards>) {
        let f = fresh();
        let boards = Rc::new(FakeBoards::default());
        *f.platform.borrow_mut() = Rc::new(FakePlatform(boards.clone()));
        (f, boards)
    }

    /// Installs a Lua callback that appends `(value, err)` to a global `seen`
    /// list, so a test can assert both what arrived and HOW MANY times.
    fn recorder(f: &Fixture) {
        f.lua
            .load(
                "seen = {}
                 function record(v, e) seen[#seen+1] = { v = v, e = e } end",
            )
            .exec()
            .unwrap();
    }

    fn seen_count(f: &Fixture) -> usize {
        f.lua.load("return #seen").eval().unwrap()
    }

    /// The whole point of installing this unconditionally: a script can
    /// always ask `steam.available()`, in every session, with no crash and
    /// no error — NullPlatform answers `false`, never raises.
    #[test]
    fn steam_available_is_always_callable_and_false_under_null_platform() {
        let f = fresh();
        let available: bool = f.lua.load("return steam.available()").eval().unwrap();
        assert!(!available);
    }

    /// Every identity getter answers nil, not an error, when there is no
    /// backend — this is the ordinary branch a script takes, not a crash.
    #[test]
    fn identity_getters_are_nil_under_null_platform() {
        let f = fresh();
        for call in [
            "steam.localUserId()",
            "steam.personaName()",
            "steam.buildId()",
            "steam.installDir()",
            "steam.betaName()",
            "steam.isFamilyShared()",
            "steam.isCybercafe()",
            "steam.uiLanguage()",
            "steam.isSteamDeck()",
            "steam.isBigPictureMode()",
        ] {
            let is_nil: bool = f.lua.load(format!("return {call} == nil")).eval().unwrap();
            assert!(is_nil, "{call} should be nil under NullPlatform");
        }
    }

    /// `drain` must never panic when nothing is registered — a project that
    /// never calls `steam.onPersonaChanged` still runs every frame.
    #[test]
    fn drain_is_a_no_op_with_no_callback_registered() {
        let f = fresh();
        drain(&f.lua, &f.platform, &f.state, &f.logs);
        assert!(f.logs.borrow().is_empty());
    }

    /// Every achievement/stat READ is nil, and `statsReady` is false, under
    /// `NullPlatform` — the ordinary "not on Steam" branch, not an error.
    #[test]
    fn achievement_and_stat_reads_are_nil_under_null_platform() {
        let f = fresh();
        let ready: bool = f.lua.load("return steam.statsReady()").eval().unwrap();
        assert!(!ready);
        for call in [
            "steam.achievementUnlocked(\"WIN\")",
            "steam.achievementGlobalPercent(\"WIN\")",
            "steam.achievementName(\"WIN\")",
            "steam.achievementDescription(\"WIN\")",
            "steam.statInt(\"kills\")",
            "steam.statFloat(\"distance\")",
        ] {
            let is_nil: bool = f.lua.load(format!("return {call} == nil")).eval().unwrap();
            assert!(is_nil, "{call} should be nil under NullPlatform");
        }
    }

    /// Every achievement/stat WRITE answers `(false, "not available")`
    /// under `NullPlatform`, rather than raising on a method that doesn't
    /// exist — a script can check `ok` without wrapping every call in
    /// `pcall`.
    #[test]
    fn achievement_and_stat_writes_fail_cleanly_under_null_platform() {
        let f = fresh();
        for call in [
            "steam.unlockAchievement(\"WIN\")",
            "steam.clearAchievement(\"WIN\")",
            "steam.setStatInt(\"kills\", 1)",
            "steam.setStatFloat(\"distance\", 1.5)",
            "steam.resetAllStats(false)",
        ] {
            let (ok, err): (bool, Option<String>) =
                f.lua.load(format!("return {call}")).eval().unwrap();
            assert!(!ok, "{call} should fail under NullPlatform");
            assert!(err.is_some_and(|e| !e.is_empty()), "{call} should carry a real message");
        }
        // flushStats is fire-and-forget — never raises, nothing to assert
        // beyond "it runs".
        f.lua.load("steam.flushStats()").exec().unwrap();
    }

    /// Every cloud READ is nil under `NullPlatform` — the ordinary "not on
    /// Steam" branch.
    #[test]
    fn cloud_reads_are_nil_under_null_platform() {
        let f = fresh();
        for call in [
            "steam.cloudEnabled()",
            "steam.cloudEnabledForAccount()",
            "steam.cloudFiles()",
            "steam.cloudFileExists(\"save.dat\")",
            "steam.cloudFileTimestamp(\"save.dat\")",
        ] {
            let is_nil: bool = f.lua.load(format!("return {call} == nil")).eval().unwrap();
            assert!(is_nil, "{call} should be nil under NullPlatform");
        }
    }

    /// Every cloud WRITE — including `cloudRead`, which answers `(nil, err)`
    /// rather than plain `nil` when the file can't be reached — carries a
    /// real error under `NullPlatform`, never raises.
    #[test]
    fn cloud_writes_fail_cleanly_under_null_platform() {
        let f = fresh();
        for call in [
            "steam.setCloudEnabled(true)",
            "steam.cloudDelete(\"save.dat\")",
            "steam.cloudForget(\"save.dat\")",
            "steam.cloudRead(\"save.dat\")",
            "steam.cloudWrite(\"save.dat\", \"data\")",
        ] {
            let (first, err): (Value, Option<String>) =
                f.lua.load(format!("return {call}")).eval().unwrap();
            assert!(
                matches!(first, Value::Boolean(false) | Value::Nil),
                "{call} should not succeed under NullPlatform"
            );
            assert!(err.is_some_and(|e| !e.is_empty()), "{call} should carry a real message");
        }
    }

    /// `friends()` and `friendRichPresence` are nil under `NullPlatform` —
    /// the ordinary "not on Steam" branch, not an error. A garbage friend id
    /// must not panic either (it fails the `u64` parse rather than the
    /// platform lookup, but the observable answer — nil, no crash — is the
    /// same either way).
    #[test]
    fn social_reads_are_nil_under_null_platform() {
        let f = fresh();
        for call in ["steam.friends()", "steam.friendRichPresence(\"76561198000000000\", \"status\")", "steam.friendRichPresence(\"not-a-number\", \"status\")"] {
            let is_nil: bool = f.lua.load(format!("return {call} == nil")).eval().unwrap();
            assert!(is_nil, "{call} should be nil under NullPlatform");
        }
    }

    /// The invariant the whole leaderboard API rests on: a callback fires
    /// **exactly once, on a later frame**, even with no Steam at all. Without
    /// this a script needs two failure paths — one for "Steam said no" and a
    /// different one for "there was no Steam to ask" — and the second is the
    /// one nobody writes.
    #[test]
    fn every_leaderboard_call_fires_its_callback_once_when_there_is_no_steam() {
        for call in [
            "steam.findLeaderboard(\"HI\", record)",
            "steam.findOrCreateLeaderboard(\"HI\", record)",
            "steam.uploadScore(\"12\", 5, record)",
            "steam.downloadScores(\"12\", record)",
        ] {
            let f = fresh();
            recorder(&f);
            f.lua.load(call).exec().unwrap();
            assert_eq!(seen_count(&f), 0, "{call} must not call back inline");

            drain(&f.lua, &f.platform, &f.state, &f.logs);
            assert_eq!(seen_count(&f), 1, "{call} should have called back once");
            let (v_nil, err): (bool, Option<String>) =
                f.lua.load("return seen[1].v == nil, seen[1].e").eval().unwrap();
            assert!(v_nil, "{call} should answer nil");
            assert!(err.is_some_and(|e| !e.is_empty()), "{call} should carry a message");

            // …and not again on the next frame.
            drain(&f.lua, &f.platform, &f.state, &f.logs);
            assert_eq!(seen_count(&f), 1, "{call} called back twice");
        }
    }

    /// A resolved board reaches Lua as a table, with its id a STRING — a
    /// handle is a full `u64` and a Lua number would round it.
    #[test]
    fn a_resolved_board_arrives_as_a_table_with_a_string_id() {
        let (f, boards) = with_boards();
        recorder(&f);
        f.lua.load("steam.findLeaderboard(\"HI\", record)").exec().unwrap();
        boards.answer(
            1,
            LeaderboardOutcome::Board(Some(LeaderboardInfo {
                id: 9_007_199_254_740_995, // past 2^53: a Lua number loses this
                name: "HI".into(),
                entry_count: 42,
                sort: Some(LeaderboardSort::Ascending),
                display: Some(LeaderboardDisplay::TimeSeconds),
            })),
        );
        drain(&f.lua, &f.platform, &f.state, &f.logs);

        let (id, name, count, sort, display): (String, String, i32, String, String) = f
            .lua
            .load(
                "local b = seen[1].v
                 return b.id, b.name, b.entryCount, b.sort, b.display",
            )
            .eval()
            .unwrap();
        assert_eq!(id, "9007199254740995", "the id must survive as an exact string");
        assert_eq!((name.as_str(), count), ("HI", 42));
        assert_eq!((sort.as_str(), display.as_str()), ("ascending", "seconds"));
    }

    /// "No board by that name" is nil with NO error — the backend answered
    /// the question successfully, and a script branches on it rather than
    /// reporting a failure that didn't happen.
    #[test]
    fn a_board_that_does_not_exist_is_nil_without_an_error() {
        let (f, boards) = with_boards();
        recorder(&f);
        f.lua.load("steam.findLeaderboard(\"NOPE\", record)").exec().unwrap();
        boards.answer(1, LeaderboardOutcome::Board(None));
        drain(&f.lua, &f.platform, &f.state, &f.logs);

        let (v_nil, e_nil): (bool, bool) =
            f.lua.load("return seen[1].v == nil, seen[1].e == nil").eval().unwrap();
        assert!(v_nil && e_nil, "a missing board is (nil, nil), not an error");
    }

    /// Downloaded rows arrive as a 1-based list, with `userId` a string for
    /// the same reason board ids are.
    #[test]
    fn downloaded_entries_arrive_as_a_list() {
        let (f, boards) = with_boards();
        recorder(&f);
        f.lua.load("steam.downloadScores(\"12\", record)").exec().unwrap();
        boards.answer(
            1,
            LeaderboardOutcome::Entries(vec![
                LeaderboardEntry {
                    user_id: 76561198000000000,
                    global_rank: 1,
                    score: 9999,
                    details: vec![7, 8],
                },
                LeaderboardEntry {
                    user_id: 76561198000000001,
                    global_rank: 2,
                    score: 10,
                    details: vec![],
                },
            ]),
        );
        drain(&f.lua, &f.platform, &f.state, &f.logs);

        let (n, id, rank, score, d1, d2, no_details): (
            usize,
            String,
            i32,
            i32,
            i32,
            i32,
            usize,
        ) = f
            .lua
            .load(
                "local r = seen[1].v
                 return #r, r[1].userId, r[1].rank, r[1].score, r[1].details[1], \
                 r[1].details[2], #r[2].details",
            )
            .eval()
            .unwrap();
        assert_eq!(n, 2);
        assert_eq!(id, "76561198000000000");
        assert_eq!((rank, score, d1, d2), (1, 9999, 7, 8));
        assert_eq!(no_details, 0, "an entry with no details gets an empty list, not nil");
    }

    /// An upload answers what was actually STORED plus both ranks — under
    /// `keepBest` the stored score is not necessarily the one uploaded, and a
    /// script showing "new personal best" needs `changed` to tell.
    #[test]
    fn an_upload_result_reports_what_was_stored() {
        let (f, boards) = with_boards();
        recorder(&f);
        f.lua.load("steam.uploadScore(\"12\", 500, record)").exec().unwrap();
        boards.answer(
            1,
            LeaderboardOutcome::Uploaded(ScoreUploaded {
                score: 900,
                changed: false,
                global_rank_new: 3,
                global_rank_previous: 3,
            }),
        );
        drain(&f.lua, &f.platform, &f.state, &f.logs);

        let (score, changed, rank, prev): (i32, bool, i32, i32) = f
            .lua
            .load("local u = seen[1].v return u.score, u.changed, u.rank, u.previousRank")
            .eval()
            .unwrap();
        assert_eq!(score, 900, "keepBest kept the better score already there");
        assert!(!changed);
        assert_eq!((rank, prev), (3, 3));
    }

    /// `count` (not an end rank) is the caller-facing shape, and a NEGATIVE
    /// start — how an around-user request asks for rows better than your own —
    /// has to survive the conversion. Getting this backwards is the off-by-one
    /// the `count` shape exists to prevent.
    #[test]
    fn count_becomes_an_end_rank_and_a_negative_start_survives() {
        let (f, boards) = with_boards();
        recorder(&f);
        f.lua
            .load(
                "steam.downloadScores(\"12\", { scope = \"aroundUser\", start = -4, \
                 count = 9 }, record)",
            )
            .exec()
            .unwrap();
        assert_eq!(
            boards.asked.borrow()[0],
            "download 12 GlobalAroundUser -4..4",
            "start -4 with count 9 spans -4..=4"
        );
    }

    /// The default download is the top ten, counted from rank 1 — a leaderboard
    /// with no options asked for should be the thing everybody means.
    #[test]
    fn the_default_download_is_the_top_ten() {
        let (f, boards) = with_boards();
        recorder(&f);
        f.lua.load("steam.downloadScores(\"12\", record)").exec().unwrap();
        assert_eq!(boards.asked.borrow()[0], "download 12 Global 1..10");
    }

    /// A handle that isn't even a number never reaches the backend, and says
    /// what to pass instead — the likeliest misuse of an API whose handles
    /// look like ordinary strings.
    #[test]
    fn a_board_handle_that_is_not_a_number_fails_without_reaching_the_backend() {
        let (f, boards) = with_boards();
        recorder(&f);
        f.lua.load("steam.uploadScore(\"board-one\", 5, record)").exec().unwrap();
        assert!(boards.asked.borrow().is_empty(), "a malformed handle must not be sent");

        drain(&f.lua, &f.platform, &f.state, &f.logs);
        let err: String = f.lua.load("return seen[1].e").eval().unwrap();
        assert!(err.contains("findLeaderboard"), "the error should say where a handle comes from: {err}");
    }

    /// Stop drops every pending callback: it closes over nodes that no longer
    /// exist, and the backend's own request is still in flight and will land.
    #[test]
    fn stop_drops_pending_leaderboard_callbacks() {
        let (f, boards) = with_boards();
        recorder(&f);
        f.lua.load("steam.findLeaderboard(\"HI\", record)").exec().unwrap();

        f.state.borrow_mut().cancel_all();
        boards.answer(1, LeaderboardOutcome::Board(None));
        drain(&f.lua, &f.platform, &f.state, &f.logs);

        assert_eq!(seen_count(&f), 0, "a callback from a stopped session must not fire");
    }

    /// `cancel_all` takes the persona callback too — same hazard, same rule.
    #[test]
    fn stop_drops_the_persona_callback() {
        let f = fresh();
        f.lua.load("steam.onPersonaChanged(function() end)").exec().unwrap();
        assert!(f.state.borrow().persona_changed_cb.is_some());
        f.state.borrow_mut().cancel_all();
        assert!(f.state.borrow().persona_changed_cb.is_none());
    }

    /// A callback that starts another request is ordinary — find a board, then
    /// immediately download it. That re-borrows the state `drain` is holding,
    /// so the borrow must be released before any callback runs.
    #[test]
    fn a_callback_may_start_another_request() {
        let (f, boards) = with_boards();
        recorder(&f);
        f.lua
            .load("steam.findLeaderboard(\"HI\", function(b, e) steam.downloadScores(\"12\", record) end)")
            .exec()
            .unwrap();
        boards.answer(1, LeaderboardOutcome::Board(None));
        drain(&f.lua, &f.platform, &f.state, &f.logs);

        assert_eq!(boards.asked.borrow().len(), 2, "the second request should have been made");
        assert_eq!(boards.asked.borrow()[1], "download 12 Global 1..10");
    }

    /// `in_flight` counts what has been asked and not yet answered — the
    /// number a "loading scores…" spinner hangs off.
    #[test]
    fn in_flight_counts_what_has_not_come_back() {
        let (f, boards) = with_boards();
        recorder(&f);
        f.lua.load("steam.findLeaderboard(\"HI\", record)").exec().unwrap();
        f.lua.load("steam.findLeaderboard(\"LO\", record)").exec().unwrap();
        let n: usize = f.lua.load("return steam.leaderboardsInFlight()").eval().unwrap();
        assert_eq!(n, 2);

        boards.answer(1, LeaderboardOutcome::Board(None));
        drain(&f.lua, &f.platform, &f.state, &f.logs);
        let n: usize = f.lua.load("return steam.leaderboardsInFlight()").eval().unwrap();
        assert_eq!(n, 1, "one answered, one still waiting");
    }

    /// An options table is read BY NAME, so a typo must be refused rather than
    /// silently taking the default (`floptle/0082`).
    #[test]
    fn an_unknown_option_key_is_refused() {
        let (f, _boards) = with_boards();
        recorder(&f);
        let e = f
            .lua
            .load("steam.downloadScores(\"12\", { scpe = \"friends\" }, record)")
            .exec()
            .unwrap_err()
            .to_string();
        assert!(e.contains("scpe"), "the error should name the key: {e}");
        assert!(e.contains("scope"), "and suggest the real one: {e}");
    }

    /// An enumerated value that isn't one of the names is refused, naming what
    /// IS accepted — the same rule as the key check, one level down.
    #[test]
    fn an_unknown_enum_value_is_refused() {
        let (f, _boards) = with_boards();
        recorder(&f);
        let e = f
            .lua
            .load("steam.downloadScores(\"12\", { scope = \"everyone\" }, record)")
            .exec()
            .unwrap_err()
            .to_string();
        assert!(e.contains("everyone") && e.contains("aroundUser"), "{e}");
    }

    /// A rank too big for an `i32` is REFUSED, not truncated. `as i32` would
    /// turn 4294967296 into 0 and quietly download the top of the board while
    /// reporting success — a wrong answer at exit 0, the shape 43% of this
    /// engine's filed bugs have taken.
    #[test]
    fn a_rank_too_big_for_an_i32_is_refused_not_truncated() {
        let (f, boards) = with_boards();
        recorder(&f);
        let e = f
            .lua
            .load("steam.downloadScores(\"12\", { start = 4294967296 }, record)")
            .exec()
            .unwrap_err()
            .to_string();
        assert!(e.contains("4294967296"), "the error should name the value: {e}");
        assert!(boards.asked.borrow().is_empty(), "nothing should have been requested");
    }

    /// `start + count` must not overflow. An `i32::MAX` start with any count
    /// would panic the whole script host in a debug build — over a number in
    /// one options table.
    #[test]
    fn an_extreme_start_does_not_overflow_the_end_rank() {
        let (f, boards) = with_boards();
        recorder(&f);
        f.lua
            .load("steam.downloadScores(\"12\", { start = 2147483647, count = 10 }, record)")
            .exec()
            .unwrap();
        assert_eq!(
            boards.asked.borrow()[0],
            "download 12 Global 2147483647..2147483647",
            "the end rank should saturate, not wrap"
        );
    }

    /// A count nobody could mean is refused, naming the range and what to do
    /// instead — refusing beats clamping for a value a script states.
    #[test]
    fn an_absurd_row_count_is_refused_with_the_range() {
        let (f, _boards) = with_boards();
        recorder(&f);
        for bad in ["0", "999999999"] {
            let e = f
                .lua
                .load(format!("steam.downloadScores(\"12\", {{ count = {bad} }}, record)"))
                .exec()
                .unwrap_err()
                .to_string();
            assert!(e.contains("10000"), "the error should state the cap: {e}");
        }
    }

    /// Forgetting the callback after an options table is a real slip, and one
    /// a silently-ignoring call would turn into "the scores never load".
    #[test]
    fn an_options_table_without_a_callback_is_refused() {
        let (f, _boards) = with_boards();
        let e = f
            .lua
            .load("steam.downloadScores(\"12\", { scope = \"friends\" })")
            .exec()
            .unwrap_err()
            .to_string();
        assert!(e.contains("callback"), "{e}");
    }

    /// `setRichPresence` carries a real error under `NullPlatform`;
    /// `clearRichPresence` is fire-and-forget and never raises.
    #[test]
    fn social_writes_fail_cleanly_under_null_platform() {
        let f = fresh();
        let (ok, err): (bool, Option<String>) =
            f.lua.load("return steam.setRichPresence(\"status\", \"In the lobby\")").eval().unwrap();
        assert!(!ok, "setRichPresence should fail under NullPlatform");
        assert!(err.is_some_and(|e| !e.is_empty()), "setRichPresence should carry a real message");
        f.lua.load("steam.clearRichPresence()").exec().unwrap();
    }
}
