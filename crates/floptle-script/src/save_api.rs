//! The Lua `save.*` API — persistent game data (roadmap A2).
//!
//! A per-slot key→value store that survives Play sessions, editor restarts, and
//! ships with exported builds. Values ride the same guardrailed [`NetValue`]
//! marshalling as `synced` vars (numbers, strings, bools, tables ≤ depth 4,
//! ≤ 1 KB each — no functions/userdata), stored as human-readable RON at
//! `<project>/save/<slot>.ron`.
//!
//! Loading is lazy (first touch reads the file); writes mark the store dirty and
//! the editor flushes on Stop + periodically during Play, so a crash loses at
//! most a few seconds. `save.flush()` forces a write (checkpoints).
//!
//! Multiplayer: this is LOCAL storage. For server-authoritative progress, call
//! `save.*` in server-side script paths (`net.isServer()`) and hand results to
//! clients via `synced`/RPC.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use floptle_net::NetValue;
use mlua::{Lua, Value};

pub(crate) struct SaveState {
    pub slot: String,
    pub store: HashMap<String, NetValue>,
    pub loaded: bool,
    pub dirty: bool,
}

impl Default for SaveState {
    fn default() -> Self {
        Self { slot: "main".into(), store: HashMap::new(), loaded: false, dirty: false }
    }
}

fn slot_path(root: &std::path::Path, slot: &str) -> PathBuf {
    root.join("save").join(format!("{slot}.ron"))
}

/// A slot name must stay a safe filename — no separators, no dots, no empties.
fn valid_slot(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn ensure_loaded(state: &mut SaveState, root: &std::path::Path) {
    if state.loaded {
        return;
    }
    state.loaded = true;
    state.store = std::fs::read_to_string(slot_path(root, &state.slot))
        .ok()
        .and_then(|text| ron::from_str(&text).ok())
        .unwrap_or_default();
}

/// Write the slot to disk if dirty. Returns an error string for the caller to log.
pub(crate) fn flush(state: &mut SaveState, root: &std::path::Path) -> Result<(), String> {
    if !state.dirty || !state.loaded {
        return Ok(());
    }
    let path = slot_path(root, &state.slot);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("save: create {dir:?}: {e}"))?;
    }
    let text = ron::ser::to_string_pretty(&state.store, ron::ser::PrettyConfig::default())
        .map_err(|e| format!("save: serialize: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("save: write {path:?}: {e}"))?;
    state.dirty = false;
    Ok(())
}

pub(crate) fn install_save_api(
    lua: &Lua,
    state: Rc<RefCell<SaveState>>,
    root: Rc<RefCell<PathBuf>>,
    logs: Rc<RefCell<Vec<crate::ScriptLog>>>,
) {
    let Ok(t) = lua.create_table() else { return };

    // save.set(key, value) — value takes the synced-var guardrails (depth ≤ 4,
    // ≤ 1 KB, no functions/userdata); a violation is a loud script error.
    {
        let state = state.clone();
        let root = root.clone();
        if let Ok(f) = lua.create_function(move |_, (key, value): (String, Value)| {
            let nv = crate::net_api::lua_to_netvalue(&value, 0)
                .map_err(|e| mlua::Error::RuntimeError(format!("save.set(\"{key}\"): {e}")))?;
            let mut s = state.borrow_mut();
            ensure_loaded(&mut s, &root.borrow());
            s.store.insert(key, nv);
            s.dirty = true;
            Ok(())
        }) {
            let _ = t.set("set", f);
        }
    }

    // save.get(key [, default]) — the stored value, else the default, else nil.
    {
        let state = state.clone();
        let root = root.clone();
        if let Ok(f) = lua.create_function(move |lua, (key, default): (String, Option<Value>)| {
            let mut s = state.borrow_mut();
            ensure_loaded(&mut s, &root.borrow());
            match s.store.get(&key) {
                Some(v) => crate::net_api::netvalue_to_lua(lua, v),
                None => Ok(default.unwrap_or(Value::Nil)),
            }
        }) {
            let _ = t.set("get", f);
        }
    }

    // save.delete(key) — true if something was removed.
    {
        let state = state.clone();
        let root = root.clone();
        if let Ok(f) = lua.create_function(move |_, key: String| {
            let mut s = state.borrow_mut();
            ensure_loaded(&mut s, &root.borrow());
            let had = s.store.remove(&key).is_some();
            s.dirty |= had;
            Ok(had)
        }) {
            let _ = t.set("delete", f);
        }
    }

    // save.deleteSlot(name) — delete a slot's store FILE from disk (save-slot
    // management UIs: "delete this save"). Deleting the ACTIVE slot also wipes
    // the in-memory store, so the slot is immediately reusable as a fresh save.
    // Returns true if a file was actually removed. Terrain a game persisted per
    // slot is its own directory — see terrain.deleteSaveDir.
    {
        let state = state.clone();
        let root = root.clone();
        if let Ok(f) = lua.create_function(move |_, name: String| {
            if !valid_slot(&name) {
                return Err(mlua::Error::RuntimeError(format!(
                    "save.deleteSlot(\"{name}\"): slot names are letters/digits/-/_ (max 64)"
                )));
            }
            let mut s = state.borrow_mut();
            if name == s.slot {
                s.store.clear();
                s.loaded = true; // a fresh, empty store — nothing to lazily read back
                s.dirty = false;
            }
            Ok(std::fs::remove_file(slot_path(&root.borrow(), &name)).is_ok())
        }) {
            let _ = t.set("deleteSlot", f);
        }
    }

    // save.slot([name]) — switch the active slot (flushing the old one first);
    // with no argument, returns the current slot's name.
    {
        let state = state.clone();
        let root = root.clone();
        let logs = logs.clone();
        if let Ok(f) = lua.create_function(move |_, name: Option<String>| {
            let mut s = state.borrow_mut();
            let Some(name) = name else { return Ok(s.slot.clone()) };
            if !valid_slot(&name) {
                return Err(mlua::Error::RuntimeError(format!(
                    "save.slot(\"{name}\"): slot names are letters/digits/-/_ (max 64)"
                )));
            }
            if name != s.slot {
                if let Err(e) = flush(&mut s, &root.borrow()) {
                    logs.borrow_mut().push(crate::ScriptLog {
                        level: crate::LogLevel::Error,
                        msg: e,
                        source: None,
                    });
                }
                s.slot = name;
                s.loaded = false;
                s.store.clear();
                s.dirty = false;
            }
            Ok(s.slot.clone())
        }) {
            let _ = t.set("slot", f);
        }
    }

    // save.flush() — force the write now (checkpoints, before risky sections).
    {
        let state = state.clone();
        let logs = logs.clone();
        if let Ok(f) = lua.create_function(move |_, ()| {
            let mut s = state.borrow_mut();
            if let Err(e) = flush(&mut s, &root.borrow()) {
                logs.borrow_mut().push(crate::ScriptLog {
                    level: crate::LogLevel::Error,
                    msg: e.clone(),
                    source: None,
                });
                return Ok(false);
            }
            Ok(true)
        }) {
            let _ = t.set("flush", f);
        }
    }

    let _ = lua.globals().set("save", t);
}
