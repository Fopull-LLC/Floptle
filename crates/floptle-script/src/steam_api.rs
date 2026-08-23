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
//! Installed unconditionally (`ScriptHost::new()`), same as every other
//! `install_*_api` — the `steam` global always exists so `steam.available()`
//! is always safe to call. What varies is only what `platform` currently
//! points at: `NullPlatform` by default, swapped for a real
//! `floptle_steam::SteamPlatform` by `ScriptHost::set_platform` when (and
//! only when) the caller has decided this session IS the game — see
//! `docs/steam-integration-proposal.md`'s "Where Steam activates".

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Lua, Value};

use crate::{LogLevel, ScriptLog};
use floptle_services::{Achievements, Cloud, Platform};

/// The platform backend, swappable after `ScriptHost::new()` via
/// `set_platform` — every Lua closure below holds a clone of this same cell,
/// so a later swap is visible to all of them without reinstalling anything.
pub(crate) type SharedPlatform = Rc<RefCell<Rc<dyn Platform>>>;

/// State private to the `steam` API: just the registered persona-change
/// callback, drained once per frame like every other event queue here.
#[derive(Default)]
pub(crate) struct SteamState {
    persona_changed_cb: Option<mlua::Function>,
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

/// Once per frame: pumps the backend's callbacks and, if the local user's
/// persona changed since the last poll, fires `steam.onPersonaChanged`.
pub(crate) fn drain(
    platform: &SharedPlatform,
    state: &Rc<RefCell<SteamState>>,
    logs: &Rc<RefCell<Vec<ScriptLog>>>,
) {
    let changed = {
        let backend = platform.borrow();
        backend.pump();
        backend.identity().map(|id| id.poll_persona_change()).unwrap_or(false)
    };
    if !changed {
        return;
    }
    let cb = state.borrow().persona_changed_cb.clone();
    if let Some(cb) = cb
        && let Err(e) = cb.call::<()>(())
    {
        log(logs, LogLevel::Error, format!("steam.onPersonaChanged callback: {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_services::NullPlatform;

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
        drain(&f.platform, &f.state, &f.logs);
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
}
