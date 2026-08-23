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
use floptle_services::Platform;

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
}
