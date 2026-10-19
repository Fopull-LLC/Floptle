//! The Lua `save.*` API — persistent game data (roadmap A2).
//!
//! A per-slot key→value store that survives Play sessions, editor restarts, and
//! ships with exported builds. Values ride the same [`NetValue`] marshalling as
//! `synced` vars (numbers, strings, bools, tables — no functions/userdata), but
//! nest up to [`SAVE_MAX_DEPTH`] rather than the wire's 4, stored as RON at
//! `<project>/save/<slot>.ron`.
//!
//! Loading is lazy (first touch reads the file); writes mark the store dirty and
//! the editor flushes on Stop + periodically during Play, so a crash loses at
//! most a few seconds. `save.flush()` forces a write (checkpoints).
//!
//! Multiplayer: this is local storage. For server-authoritative progress, call
//! `save.*` in server-side script paths (`net.isServer()`) and hand results to
//! clients via `synced`/RPC.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use floptle_net::NetValue;
use mlua::{Lua, Value};

/// How many keys one slot may hold, and how many bytes between them. Without
/// these the count was unbounded, and the
/// flush serialises the whole store every few seconds — a slot that only grew
/// was a stall that only grew with it.
pub const MAX_SAVE_KEYS: usize = 10_000;
pub const MAX_SAVE_BYTES: usize = 4 * 1024 * 1024;

pub(crate) struct SaveState {
    pub slot: String,
    pub store: HashMap<String, NetValue>,
    pub loaded: bool,
    pub dirty: bool,
    /// The store's size on the wire, kept alongside it so a `set` can answer
    /// "would this fit" without re-encoding ten thousand values.
    pub bytes: usize,
}

impl Default for SaveState {
    fn default() -> Self {
        Self { slot: "main".into(), store: HashMap::new(), loaded: false, dirty: false, bytes: 0 }
    }
}

impl SaveState {
    /// Put one value in, or say why it does not fit. The caps are on the slot
    /// as a whole, so a key that replaces one already there is charged the
    /// difference.
    pub(crate) fn insert(&mut self, key: String, nv: NetValue) -> Result<(), String> {
        let incoming = nv.encoded_len();
        let outgoing = self.store.get(&key).map_or(0, NetValue::encoded_len);
        let keys_after = self.store.len() + usize::from(outgoing == 0 && !self.store.contains_key(&key));
        if keys_after > MAX_SAVE_KEYS {
            return Err(format!(
                "the slot already holds {MAX_SAVE_KEYS} keys. A save is for what the player \
                 did, not for everything the game computed — put a table under one key, or \
                 delete what is stale"
            ));
        }
        let bytes_after = self.bytes.saturating_sub(outgoing).saturating_add(incoming);
        if bytes_after > MAX_SAVE_BYTES {
            return Err(format!(
                "the slot would be {bytes_after} bytes, more than the {MAX_SAVE_BYTES} it may hold"
            ));
        }
        self.store.insert(key, nv);
        self.bytes = bytes_after;
        self.dirty = true;
        Ok(())
    }

    /// Take one value out, keeping the byte count honest.
    pub(crate) fn remove(&mut self, key: &str) -> Option<NetValue> {
        let gone = self.store.remove(key)?;
        self.bytes = self.bytes.saturating_sub(gone.encoded_len());
        self.dirty = true;
        Some(gone)
    }

    fn recount(&mut self) {
        self.bytes = self.store.values().map(NetValue::encoded_len).sum();
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

/// How deep a saved table may nest. Bounded at all because a cyclic table
/// would otherwise recurse until the stack goes, and kept well inside what the
/// slot file's own reader accepts, so nothing written can fail to load.
pub(crate) const SAVE_MAX_DEPTH: usize = 16;

fn ensure_loaded(state: &mut SaveState, root: &std::path::Path) {
    if state.loaded {
        return;
    }
    state.loaded = true;
    state.store = floptle_vfs::read_to_string(slot_path(root, &state.slot))
        .ok()
        .and_then(|text| ron::from_str(&text).ok())
        .unwrap_or_default();
    state.recount();
}

/// Write the slot to disk if dirty. Returns an error string for the caller to log.
pub(crate) fn flush(state: &mut SaveState, root: &std::path::Path) -> Result<(), String> {
    if !state.dirty || !state.loaded {
        return Ok(());
    }
    let path = slot_path(root, &state.slot);
    if let Some(dir) = path.parent() {
        floptle_vfs::create_dir_all(dir).map_err(|e| format!("save: create {dir:?}: {e}"))?;
    }
    let text = ron::ser::to_string_pretty(&state.store, ron::ser::PrettyConfig::default())
        .map_err(|e| format!("save: serialize: {e}"))?;
    floptle_vfs::write(&path, text).map_err(|e| format!("save: write {path:?}: {e}"))?;
    state.dirty = false;
    Ok(())
}

pub(crate) fn install_save_api(
    lua: &Lua,
    state: Rc<RefCell<SaveState>>,
    paths: crate::paths::Paths,
    logs: Rc<RefCell<Vec<crate::ScriptLog>>>,
) {
    let Ok(t) = lua.create_table() else { return };

    // save.set(key, value) — plain data only (no functions/userdata), nested
    // up to SAVE_MAX_DEPTH and within the slot's byte cap; a violation is a
    // loud script error. Not the wire's depth-4 rule: a save stays on this
    // machine, and a craft with parts with modules with state is four deep
    // before it says anything.
    {
        let state = state.clone();
        let paths = paths.clone();
        if let Ok(f) = lua.create_function(move |_, (key, value): (String, Value)| {
            let nv = crate::net_api::lua_to_netvalue_max(&value, 0, SAVE_MAX_DEPTH)
                .map_err(|e| mlua::Error::RuntimeError(format!("save.set(\"{key}\"): {e}")))?;
            let mut s = state.borrow_mut();
            ensure_loaded(&mut s, &paths.runtime_base());
            s.insert(key.clone(), nv)
                .map_err(|e| mlua::Error::RuntimeError(format!("save.set(\"{key}\"): {e}")))
        }) {
            let _ = t.set("set", f);
        }
    }

    // save.get(key [, default]) — the stored value, else the default, else nil.
    {
        let state = state.clone();
        let paths = paths.clone();
        if let Ok(f) = lua.create_function(move |lua, (key, default): (String, Option<Value>)| {
            let mut s = state.borrow_mut();
            ensure_loaded(&mut s, &paths.runtime_base());
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
        let paths = paths.clone();
        if let Ok(f) = lua.create_function(move |_, key: String| {
            let mut s = state.borrow_mut();
            ensure_loaded(&mut s, &paths.runtime_base());
            Ok(s.remove(&key).is_some())
        }) {
            let _ = t.set("delete", f);
        }
    }

    // save.deleteSlot(name) — delete a slot's store file from disk (save-slot
    // management UIs: "delete this save"). Deleting the active slot also wipes
    // the in-memory store, so the slot is immediately reusable as a fresh save.
    // Returns true if a file was actually removed. Terrain a game persisted per
    // slot is its own directory — see terrain.deleteSaveDir.
    {
        let state = state.clone();
        let paths = paths.clone();
        if let Ok(f) = lua.create_function(move |_, name: String| {
            if !valid_slot(&name) {
                return Err(mlua::Error::RuntimeError(format!(
                    "save.deleteSlot(\"{name}\"): slot names are letters/digits/-/_ (max 64)"
                )));
            }
            let mut s = state.borrow_mut();
            if name == s.slot {
                s.store.clear();
                s.bytes = 0;
                s.loaded = true; // a fresh, empty store — nothing to lazily read back
                s.dirty = false;
            }
            Ok(floptle_vfs::remove_file(slot_path(&paths.runtime_base(), &name)).is_ok())
        }) {
            let _ = t.set("deleteSlot", f);
        }
    }

    // save.slot([name]) — switch the active slot (flushing the old one first);
    // with no argument, returns the current slot's name.
    {
        let state = state.clone();
        let paths = paths.clone();
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
                if let Err(e) = flush(&mut s, &paths.runtime_base()) {
                    logs.borrow_mut().push(crate::ScriptLog {
                        level: crate::LogLevel::Error,
                        msg: e,
                        source: None,
                    });
                }
                s.slot = name;
                s.loaded = false;
                s.store.clear();
                s.bytes = 0;
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
            if let Err(e) = flush(&mut s, &paths.runtime_base()) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn lua_with_save(root: &std::path::Path) -> (Lua, Rc<RefCell<SaveState>>) {
        let lua = Lua::new();
        let state = Rc::new(RefCell::new(SaveState::default()));
        let logs = Rc::new(RefCell::new(Vec::new()));
        install_save_api(&lua, state.clone(), crate::paths::Paths::at(root.to_path_buf()), logs);
        (lua, state)
    }

    /// **A save keeps nested data, and reads it back from disk.** The wire's
    /// depth-4 rule used to apply here, so a craft with parts with modules with
    /// state was refused. The deepest table allowed must also survive the
    /// write and a fresh read of the file, or a save would load as empty; one
    /// level deeper (or a cycle) is still refused, loudly.
    #[test]
    fn a_deeply_nested_table_saves_and_loads_back_from_disk() {
        let root = std::env::temp_dir().join(format!("floptle-savedeep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (lua, state) = lua_with_save(&root);
        let nest = |depth: usize| format!("local t = 'leaf' for i = 1, {depth} do t = {{ t, n = i }} end return t");
        lua.load(format!("save.set('ten', (function() {} end)())", nest(10))).exec().unwrap();
        lua.load(format!("save.set('deepest', (function() {} end)())", nest(SAVE_MAX_DEPTH))).exec().unwrap();
        let e = lua.load(format!("save.set('too', (function() {} end)())", nest(SAVE_MAX_DEPTH + 1))).exec().unwrap_err().to_string();
        assert!(e.contains("save.set(\"too\")") && e.contains("nests deeper"), "{e}");
        let e = lua.load("local c = {} c.self = c save.set('cycle', c)").exec().unwrap_err().to_string();
        assert!(e.contains("cyclic"), "{e}");
        flush(&mut state.borrow_mut(), &root).unwrap();

        let (lua, _state) = lua_with_save(&root);
        let read = "local t, d = save.get(KEY), 0 while type(t) == 'table' do d = d + 1 t = t[1] end return d, t";
        let (d, leaf): (usize, String) = lua.load(read.replace("KEY", "'ten'")).eval().unwrap();
        assert_eq!((d, leaf.as_str()), (10, "leaf"), "the 10-deep table did not come back");
        let (d, _): (usize, String) = lua.load(read.replace("KEY", "'deepest'")).eval().unwrap();
        assert_eq!(d, SAVE_MAX_DEPTH, "the deepest table allowed did not survive the file");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A slot has a ceiling on keys and on bytes**, and both are the same
    /// kind of loud error a too-big value already was. Deleting makes room
    /// again, and replacing a key is charged the difference, not the sum.
    #[test]
    fn a_slot_refuses_the_key_and_the_byte_past_its_ceiling_and_frees_on_delete() {
        let root = std::env::temp_dir().join(format!("floptle-savecap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (lua, state) = lua_with_save(&root);
        lua.load(format!("for i = 1, {MAX_SAVE_KEYS} do save.set('k' .. i, i) end")).exec().unwrap();
        assert_eq!(state.borrow().store.len(), MAX_SAVE_KEYS);
        let e = lua.load("save.set('one_more', 1)").exec().unwrap_err().to_string();
        assert!(e.contains("save.set(\"one_more\")") && e.contains("10000 keys"), "{e}");
        // Replacing an existing key is fine; so is one more after a delete.
        lua.load("save.set('k1', 'replaced')").exec().unwrap();
        lua.load("save.delete('k2') save.set('one_more', 1)").exec().unwrap();
        assert_eq!(state.borrow().store.len(), MAX_SAVE_KEYS);

        // Bytes: values just under the per-value cap, until the slot is full.
        let (lua, state) = lua_with_save(&root);
        let r = lua
            .load("for i = 1, 100000 do save.set('big' .. i, string.rep('x', 1000)) end")
            .exec();
        let e = r.unwrap_err().to_string();
        assert!(e.contains("more than the") && e.contains("bytes"), "{e}");
        let bytes = state.borrow().bytes;
        assert!(bytes <= MAX_SAVE_BYTES && bytes > MAX_SAVE_BYTES - 2048, "{bytes}");
        let counted: usize = state.borrow().store.values().map(NetValue::encoded_len).sum();
        assert_eq!(bytes, counted, "the running total drifted from the store");
        let _ = std::fs::remove_dir_all(&root);
    }
}
