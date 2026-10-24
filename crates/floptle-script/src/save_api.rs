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
//! **A flush costs the frame a list of the keys that changed.** The values go
//! to a writer thread as shared pointers, and the writer keeps every key's
//! encoded text from the last write, so it encodes only what changed and
//! joins the rest. The file is the same RON map it always was, written
//! without indentation: every save an older engine wrote loads, and every
//! save this one writes loads in an older engine. It replaces the old file by
//! rename ([`floptle_vfs::write_atomic`]), so a kill mid-write leaves the
//! previous save whole. A page has no threads, and writes on its own frame.
//!
//! **A slot is for progress and settings, not for everything a player made.**
//! It is loaded whole on the first touch and held in memory, and in a browser
//! the whole origin's storage is a few megabytes, so it stays capped at
//! [`MAX_SAVE_BYTES`]. A blueprint library, a map or a recording is a file:
//! `assets.writeJson("user://…")` / `assets.writeBytes`, which have no cap of
//! their own and replace the file the same all-or-nothing way.
//!
//! Multiplayer: this is local storage. For server-authoritative progress, call
//! `save.*` in server-side script paths (`net.isServer()`) and hand results to
//! clients via `synced`/RPC.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use floptle_net::NetValue;
use mlua::{Lua, Value};

/// How many keys one slot may hold, and how many bytes between them. Without
/// these the count was unbounded, and a slot is loaded whole and kept in
/// memory — a slot that only grew was a load that only grew with it. In a
/// browser the slot lives in storage capped at a few megabytes for the whole
/// page. Large player data belongs in a `user://` file (see the module docs).
pub const MAX_SAVE_KEYS: usize = 10_000;
pub const MAX_SAVE_BYTES: usize = 4 * 1024 * 1024;

/// One stored value, shared with the writer, and its size on the wire —
/// counted once when it goes in, so replacing it is charged without
/// re-encoding what it replaces.
#[derive(Clone)]
pub(crate) struct Entry {
    pub value: Arc<NetValue>,
    pub bytes: usize,
}

pub(crate) struct SaveState {
    pub slot: String,
    pub store: HashMap<String, Entry>,
    pub loaded: bool,
    pub dirty: bool,
    /// The store's size on the wire, kept alongside it so a `set` can answer
    /// "would this fit" without re-encoding ten thousand values.
    pub bytes: usize,
    /// Keys set or deleted since the last snapshot went to the writer.
    changed: HashSet<String>,
    /// The next snapshot carries every key, not only the changed ones: the
    /// slot was just loaded or switched, or a write failed and the writer's
    /// copy can no longer be trusted to match.
    whole: bool,
    /// The slot's file could not be read nor copied aside: nothing is
    /// written over it this session.
    blocked: bool,
    writer: Writer,
}

impl Default for SaveState {
    fn default() -> Self {
        Self {
            slot: "main".into(),
            store: HashMap::new(),
            loaded: false,
            dirty: false,
            bytes: 0,
            changed: HashSet::new(),
            whole: true,
            blocked: false,
            writer: Writer::default(),
        }
    }
}

impl SaveState {
    /// Put one value in, or say why it does not fit. The caps are on the slot
    /// as a whole, so a key that replaces one already there is charged the
    /// difference.
    pub(crate) fn insert(&mut self, key: String, nv: NetValue) -> Result<(), String> {
        let incoming = nv.encoded_len();
        let present = self.store.get(&key);
        let outgoing = present.map_or(0, |e| e.bytes);
        let keys_after = self.store.len() + usize::from(present.is_none());
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
                "the slot would be {bytes_after} bytes, more than the {MAX_SAVE_BYTES} it may hold. \
                 Something the player made (a blueprint library, a map, a recording) belongs in \
                 a file of its own — assets.writeJson(\"user://…\", value) has no cap"
            ));
        }
        self.store.insert(key.clone(), Entry { value: Arc::new(nv), bytes: incoming });
        self.changed.insert(key);
        self.bytes = bytes_after;
        self.dirty = true;
        Ok(())
    }

    /// Take one value out, keeping the byte count honest.
    pub(crate) fn remove(&mut self, key: &str) -> Option<Entry> {
        let gone = self.store.remove(key)?;
        self.bytes = self.bytes.saturating_sub(gone.bytes);
        self.changed.insert(key.to_string());
        self.dirty = true;
        Some(gone)
    }

    /// Forget the slot in memory: what is on disk is read again on the next
    /// touch (`fresh == false`), or the slot is known empty (`fresh`).
    fn reset(&mut self, fresh: bool) {
        self.store.clear();
        self.changed.clear();
        self.bytes = 0;
        self.loaded = fresh;
        self.dirty = false;
        self.whole = true;
        self.blocked = false;
    }

    /// What the writer needs for the next write: the keys that changed, each
    /// with its new value or `None` for gone, or every key. Pointers, not
    /// copies — this is the part of a flush the frame pays for.
    fn snapshot(&mut self, root: &Path) -> Job {
        let changes: Vec<(String, Option<Arc<NetValue>>)> = if self.whole {
            self.changed.clear();
            self.store.iter().map(|(k, e)| (k.clone(), Some(e.value.clone()))).collect()
        } else {
            self.changed
                .drain()
                .map(|k| {
                    let v = self.store.get(&k).map(|e| e.value.clone());
                    (k, v)
                })
                .collect()
        };
        let whole = std::mem::take(&mut self.whole);
        self.dirty = false;
        Job { path: slot_path(root, &self.slot), whole, changes }
    }

    /// Fold finished writes back in. A failed one leaves the store as it is
    /// and dirty, with the next write carrying every key, so nothing the
    /// script set is ever only in a write that did not happen.
    fn settle(&mut self, errors: Vec<String>) -> Result<(), String> {
        if errors.is_empty() {
            return Ok(());
        }
        self.whole = true;
        self.dirty = self.loaded;
        Err(errors.join("; "))
    }

    /// Wait for every write handed over so far. A slot is read, or a file
    /// removed, only once nothing is still on its way to it.
    fn drain(&mut self) -> Result<(), String> {
        let errors = self.writer.wait();
        self.settle(errors)
    }

    #[cfg(test)]
    pub(crate) fn last_write(&self) -> Option<WriteStats> {
        self.writer.last
    }
}

fn slot_path(root: &Path, slot: &str) -> PathBuf {
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

fn log_error(logs: &Rc<RefCell<Vec<crate::ScriptLog>>>, msg: String) {
    logs.borrow_mut().push(crate::ScriptLog { level: crate::LogLevel::Error, msg, source: None });
}

fn ensure_loaded(state: &mut SaveState, root: &Path, logs: &Rc<RefCell<Vec<crate::ScriptLog>>>) {
    if state.loaded {
        return;
    }
    // A write of this very slot may still be on its way (switched away and
    // back): read what it wrote, not what it replaced.
    if let Err(e) = state.drain() {
        log_error(logs, e);
    }
    state.reset(true);
    let path = slot_path(root, &state.slot);
    if !floptle_vfs::is_file(&path) {
        return;
    }
    let read = floptle_vfs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|text| ron::from_str::<HashMap<String, NetValue>>(&text).map_err(|e| e.to_string()));
    match read {
        Ok(map) => {
            state.store = map
                .into_iter()
                .map(|(k, v)| {
                    let bytes = v.encoded_len();
                    (k, Entry { value: Arc::new(v), bytes })
                })
                .collect();
            state.bytes = state.store.values().map(|e| e.bytes).sum();
        }
        // The slot starts empty, and the next flush would replace the file
        // with that. It is the player's, so it goes aside first, whole.
        Err(why) => {
            let kept = keep_aside(&path);
            log_error(
                logs,
                match &kept {
                    Some(aside) => format!(
                        "save: slot \"{}\" could not be read ({why}). It starts empty; the file \
                         was kept as {}",
                        state.slot,
                        aside.display()
                    ),
                    None => format!(
                        "save: slot \"{}\" could not be read ({why}), and could not be copied \
                         aside either. It starts empty, and is not written until it can be",
                        state.slot
                    ),
                },
            );
            state.blocked = kept.is_none();
        }
    }
}

/// Copy an unreadable slot to the first free `<slot>.ron.unreadable[-n]`
/// beside it, so a second bad file does not replace the first.
fn keep_aside(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    (1..=100).find_map(|n| {
        let aside = if n == 1 {
            path.with_file_name(format!("{name}.unreadable"))
        } else {
            path.with_file_name(format!("{name}.unreadable-{n}"))
        };
        (!floptle_vfs::exists(&aside) && floptle_vfs::copy(path, &aside).is_ok()).then_some(aside)
    })
}

/// Hand the slot to the writer if dirty, and report any write that failed
/// since the last call. With `wait`, return only once everything handed
/// over is on disk (Stop, quit, a slot switch, `save.flush{ wait = true }`).
pub(crate) fn flush(state: &mut SaveState, root: &Path, wait: bool) -> Result<(), String> {
    let mut errors = state.writer.poll();
    if state.dirty && state.loaded && !state.blocked {
        let job = state.snapshot(root);
        state.writer.submit(job);
    }
    if wait {
        errors.extend(state.writer.wait());
    }
    state.settle(errors)
}

/// One write: where, whether it is every key, and the keys that changed.
pub(crate) struct Job {
    path: PathBuf,
    whole: bool,
    changes: Vec<(String, Option<Arc<NetValue>>)>,
}

/// What one write did — counted, so a test can say which work it did.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WriteStats {
    /// Values encoded for this write; the rest were the last write's text.
    pub encoded: usize,
    /// The file's size.
    pub bytes: usize,
}

thread_local! {
    /// Values encoded on this thread. A flush that encodes on the frame's
    /// thread is the stall this module exists to avoid, and a count says so
    /// on any machine where a stopwatch would not.
    static ENCODED_HERE: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn encoded_on_this_thread() -> usize {
    ENCODED_HERE.with(Cell::get)
}

/// The writer's side: each key's encoded `"key":value` text as of the last
/// write, for one slot file.
#[derive(Default)]
struct Fragments {
    path: Option<PathBuf>,
    by_key: HashMap<String, String>,
}

impl Fragments {
    fn apply(&mut self, job: Job) -> Result<WriteStats, String> {
        if job.whole {
            self.by_key.clear();
            self.path = Some(job.path.clone());
        } else if self.path.as_ref() != Some(&job.path) {
            // Only the changed keys, against text for another file (or none):
            // writing that would drop every other key. Refuse; the failure
            // makes the next write carry the whole slot.
            return Err(format!("save: {:?} needs a whole write first", job.path));
        }
        let mut stats = WriteStats::default();
        for (key, value) in job.changes {
            match value {
                Some(v) => match encode_entry(&key, &v) {
                    Ok(text) => {
                        stats.encoded += 1;
                        self.by_key.insert(key, text);
                    }
                    Err(e) => {
                        self.path = None;
                        return Err(format!("save: serialize \"{key}\": {e}"));
                    }
                },
                None => {
                    self.by_key.remove(&key);
                }
            }
        }
        // Sorted, so the same store always writes the same file.
        let mut keys: Vec<&String> = self.by_key.keys().collect();
        keys.sort_unstable();
        let mut text = String::with_capacity(2 + self.by_key.values().map(|t| t.len() + 1).sum::<usize>());
        text.push('{');
        for (i, k) in keys.into_iter().enumerate() {
            if i > 0 {
                text.push(',');
            }
            text.push_str(&self.by_key[k]);
        }
        text.push('}');
        stats.bytes = text.len();
        if let Some(dir) = job.path.parent() {
            floptle_vfs::create_dir_all(dir).map_err(|e| format!("save: create {dir:?}: {e}"))?;
        }
        floptle_vfs::write_atomic(&job.path, text).map_err(|e| format!("save: write {:?}: {e}", job.path))?;
        Ok(stats)
    }
}

/// One map entry of the slot file, exactly as the whole-map writer spells it.
fn encode_entry(key: &str, value: &NetValue) -> Result<String, ron::Error> {
    ENCODED_HERE.with(|c| c.set(c.get() + 1));
    let mut text = ron::to_string(key)?;
    text.push(':');
    text.push_str(&ron::to_string(value)?);
    Ok(text)
}

/// The writer thread: jobs in order, one result back for each. It lives as
/// long as the slot state, and dropping it waits for what it was handed.
#[cfg(not(target_arch = "wasm32"))]
struct Worker {
    jobs: Option<std::sync::mpsc::Sender<Job>>,
    done: std::sync::mpsc::Receiver<Result<WriteStats, String>>,
    thread: Option<std::thread::JoinHandle<()>>,
    pending: usize,
}

#[cfg(not(target_arch = "wasm32"))]
impl Worker {
    fn start() -> std::io::Result<Self> {
        let (jobs, rx) = std::sync::mpsc::channel::<Job>();
        let (tx, done) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new().name("save-writer".into()).spawn(move || {
            let mut frags = Fragments::default();
            for job in rx {
                if tx.send(frags.apply(job)).is_err() {
                    break;
                }
            }
        })?;
        Ok(Self { jobs: Some(jobs), done, thread: Some(thread), pending: 0 })
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for Worker {
    fn drop(&mut self) {
        // Closing the queue ends the loop once it is empty; the join is what
        // keeps a host that is going away from leaving a write half-done for
        // the next one to read past.
        self.jobs = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[derive(Default)]
struct Writer {
    #[cfg(not(target_arch = "wasm32"))]
    worker: Option<Worker>,
    /// A page has no threads: the writer's text lives here and writes inline.
    #[cfg(target_arch = "wasm32")]
    frags: Fragments,
    /// Where writes go when no thread can be had.
    #[cfg(not(target_arch = "wasm32"))]
    inline: Fragments,
    /// Failures not yet reported.
    errors: Vec<String>,
    #[cfg(test)]
    last: Option<WriteStats>,
}

impl Writer {
    fn record(&mut self, r: Result<WriteStats, String>) {
        match r {
            #[cfg(test)]
            Ok(s) => self.last = Some(s),
            #[cfg(not(test))]
            Ok(_) => {}
            Err(e) => self.errors.push(e),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn submit(&mut self, job: Job) {
        if self.worker.is_none() {
            match Worker::start() {
                Ok(w) => self.worker = Some(w),
                Err(_) => {
                    // No thread to be had: this frame writes it, rather than
                    // nothing doing. A partial job against text this side does
                    // not hold is refused, and the retry carries every key.
                    let r = self.inline.apply(job);
                    self.record(r);
                    return;
                }
            }
        }
        let Some(w) = self.worker.as_mut() else { return };
        let job = match w.jobs.as_ref() {
            Some(tx) => match tx.send(job) {
                Ok(()) => {
                    w.pending += 1;
                    return;
                }
                Err(back) => back.0,
            },
            None => job,
        };
        // The thread is gone: write here, and start a new one next time.
        self.worker = None;
        let r = self.inline.apply(job);
        self.record(r);
    }

    #[cfg(target_arch = "wasm32")]
    fn submit(&mut self, job: Job) {
        let r = self.frags.apply(job);
        self.record(r);
    }

    /// Results of the writes that have finished, without waiting.
    fn poll(&mut self) -> Vec<String> {
        #[cfg(not(target_arch = "wasm32"))]
        while let Some(w) = self.worker.as_mut()
            && w.pending > 0
        {
            match w.done.try_recv() {
                Ok(r) => {
                    w.pending -= 1;
                    self.record(r);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.worker = None;
                    self.errors.push("save: the writer stopped; the next flush writes the whole slot".into());
                }
            }
        }
        std::mem::take(&mut self.errors)
    }

    /// Results of every write handed over, waiting for the last one.
    fn wait(&mut self) -> Vec<String> {
        #[cfg(not(target_arch = "wasm32"))]
        while let Some(w) = self.worker.as_mut()
            && w.pending > 0
        {
            match w.done.recv() {
                Ok(r) => {
                    w.pending -= 1;
                    self.record(r);
                }
                Err(_) => {
                    self.worker = None;
                    self.errors.push("save: the writer stopped; the next flush writes the whole slot".into());
                }
            }
        }
        std::mem::take(&mut self.errors)
    }
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
        let logs = logs.clone();
        if let Ok(f) = lua.create_function(move |_, (key, value): (String, Value)| {
            let nv = crate::net_api::lua_to_netvalue_max(&value, 0, SAVE_MAX_DEPTH)
                .map_err(|e| mlua::Error::RuntimeError(format!("save.set(\"{key}\"): {e}")))?;
            let mut s = state.borrow_mut();
            ensure_loaded(&mut s, &paths.runtime_base(), &logs);
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
        let logs = logs.clone();
        if let Ok(f) = lua.create_function(move |lua, (key, default): (String, Option<Value>)| {
            let mut s = state.borrow_mut();
            ensure_loaded(&mut s, &paths.runtime_base(), &logs);
            match s.store.get(&key) {
                Some(e) => crate::net_api::netvalue_to_lua(lua, &e.value),
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
        let logs = logs.clone();
        if let Ok(f) = lua.create_function(move |_, key: String| {
            let mut s = state.borrow_mut();
            ensure_loaded(&mut s, &paths.runtime_base(), &logs);
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
        let logs = logs.clone();
        if let Ok(f) = lua.create_function(move |_, name: String| {
            if !valid_slot(&name) {
                return Err(mlua::Error::RuntimeError(format!(
                    "save.deleteSlot(\"{name}\"): slot names are letters/digits/-/_ (max 64)"
                )));
            }
            let mut s = state.borrow_mut();
            // A write still on its way would put the file straight back.
            if let Err(e) = s.drain() {
                log_error(&logs, e);
            }
            if name == s.slot {
                // A fresh, empty store — nothing to lazily read back.
                s.reset(true);
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
                // Waited for: the old slot's store is about to be dropped, and
                // a failure has to be heard while it can still be named.
                if let Err(e) = flush(&mut s, &paths.runtime_base(), true) {
                    log_error(&logs, e);
                }
                s.slot = name;
                s.reset(false);
            }
            Ok(s.slot.clone())
        }) {
            let _ = t.set("slot", f);
        }
    }

    // save.flush([{ wait = true }]) — hand the slot to the writer now
    // (checkpoints, before risky sections). It returns at once and the write
    // finishes on the writer's thread; `wait` returns only once it is on disk.
    // False when a write failed (the Console says why); the slot stays dirty
    // and the next flush tries again.
    {
        let state = state.clone();
        let logs = logs.clone();
        if let Ok(f) = lua.create_function(move |_, opts: Option<mlua::Table>| {
            let wait = match &opts {
                Some(o) => {
                    crate::opts::check_keys(o, &["wait"], "save.flush")?;
                    crate::opts::opt_bool(o, "save.flush", "wait")?.unwrap_or(false)
                }
                None => false,
            };
            let mut s = state.borrow_mut();
            if let Err(e) = flush(&mut s, &paths.runtime_base(), wait) {
                log_error(&logs, e);
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
        flush(&mut state.borrow_mut(), &root, true).unwrap();

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
        let counted: usize = state.borrow().store.values().map(|e| e.value.encoded_len()).sum();
        assert_eq!(bytes, counted, "the running total drifted from the store");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A thousand parts, each a few tables deep: the shape of a craft.
    const CRAFT: &str = "local parts = {} for i = 1, 1000 do parts[i] = { id = i, kind = 'hull_' .. (i % 7), \
        pos = { x = i * 0.5, y = i * 0.25, z = -i }, rot = { x = 0, y = 0.7071, z = 0, w = 0.7071 }, mass = 12.5, \
        modules = { { name = 'tank', fuel = 100, cap = 200 }, { name = 'engine', thrust = 5000, on = true } } } end \
        return { name = 'big craft', parts = parts }";

    fn read_back(root: &std::path::Path) -> HashMap<String, NetValue> {
        // The reader every engine before this one used, on purpose: a save
        // this engine writes has to load in the one the player had yesterday.
        ron::from_str(&std::fs::read_to_string(root.join("save/main.ron")).unwrap()).unwrap()
    }

    /// **A flush costs the frame no encoding, and the writer encodes only
    /// what changed.** The flush used to pretty-print the whole store on the
    /// frame, every few seconds, so a 1,000-part craft in the slot was a
    /// stalled frame every autosave even when only a setting had changed.
    /// Counted, not timed: the frame's thread encodes nothing at all, and
    /// the writer encodes every key once, then one key for one change.
    #[test]
    fn a_flush_encodes_nothing_on_the_frame_and_only_what_changed_on_the_writer() {
        let root = std::env::temp_dir().join(format!("floptle-saveworker-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (lua, state) = lua_with_save(&root);
        lua.load(format!("save.set('craft', (function() {CRAFT} end)())")).exec().unwrap();
        lua.load("for i = 1, 50 do save.set('opt_' .. i, i) end").exec().unwrap();

        let before = encoded_on_this_thread();
        assert!(lua.load("return save.flush()").eval::<bool>().unwrap());
        assert_eq!(encoded_on_this_thread(), before, "the flush encoded on the frame's thread");
        state.borrow_mut().drain().unwrap();
        assert_eq!(state.borrow().last_write().map(|w| w.encoded), Some(51), "the first write is every key");

        lua.load("save.set('opt_7', 'changed') save.delete('opt_8') save.flush()").exec().unwrap();
        assert_eq!(encoded_on_this_thread(), before);
        state.borrow_mut().drain().unwrap();
        assert_eq!(
            state.borrow().last_write().map(|w| w.encoded),
            Some(1),
            "one changed key re-encoded more than itself"
        );

        let disk = read_back(&root);
        assert_eq!(disk.len(), 50);
        assert_eq!(disk.get("opt_7"), Some(&NetValue::Str("changed".into())));
        assert!(!disk.contains_key("opt_8"), "a deleted key came back");
        assert_eq!(disk.get("craft"), Some(&*state.borrow().store["craft"].value));

        // And a fresh session reads it all back through the API.
        let (lua, _state) = lua_with_save(&root);
        let n: usize = lua.load("return #save.get('craft').parts").eval().unwrap();
        assert_eq!(n, 1000);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A save from an older engine loads, and stays loadable by one.** The
    /// older engine wrote the map indented; this one writes it without
    /// indentation, a key at a time. Both are the same RON map, so the
    /// store reads the old file, and the old reader reads the new one.
    #[test]
    fn an_older_engines_save_loads_and_a_new_save_reads_with_the_old_reader() {
        let root = std::env::temp_dir().join(format!("floptle-saveold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut old: HashMap<String, NetValue> = HashMap::new();
        old.insert("gold".into(), NetValue::Num(42.0));
        old.insert("name with \"quotes\" and :,{}".into(), NetValue::Str("line\nbreak".into()));
        old.insert(
            "who".into(),
            NetValue::Table(vec![(NetValue::Str("hp".into()), NetValue::Num(7.5)), (NetValue::Num(1.0), NetValue::Bool(true))]),
        );
        std::fs::create_dir_all(root.join("save")).unwrap();
        let text = ron::ser::to_string_pretty(&old, ron::ser::PrettyConfig::default()).unwrap();
        std::fs::write(root.join("save/main.ron"), text).unwrap();

        let (lua, state) = lua_with_save(&root);
        let (gold, hp): (f64, f64) = lua.load("return save.get('gold'), save.get('who').hp").eval().unwrap();
        assert_eq!((gold, hp), (42.0, 7.5), "the older engine's save did not load");
        lua.load("save.set('added', 'new') save.flush({ wait = true })").exec().unwrap();
        old.insert("added".into(), NetValue::Str("new".into()));
        assert_eq!(read_back(&root), old, "the old reader does not read what this engine wrote");
        assert_eq!(state.borrow().bytes, old.values().map(NetValue::encoded_len).sum::<usize>());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A slot file that cannot be read is put aside before anything is
    /// written over it.** It used to read as an empty slot, and the next
    /// flush replaced the player's save with whatever had been set since.
    #[test]
    fn an_unreadable_slot_is_kept_aside_before_the_slot_is_written() {
        let root = std::env::temp_dir().join(format!("floptle-saveunread-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("save")).unwrap();
        std::fs::write(root.join("save/main.ron"), "{\"gold\": Num(4").unwrap();
        let lua = Lua::new();
        let state = Rc::new(RefCell::new(SaveState::default()));
        let logs = Rc::new(RefCell::new(Vec::new()));
        install_save_api(&lua, state.clone(), crate::paths::Paths::at(root.clone()), logs.clone());
        lua.load("save.set('fresh', 1) save.flush({ wait = true })").exec().unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("save/main.ron.unreadable")).ok().as_deref(),
            Some("{\"gold\": Num(4"),
            "the unreadable save was not kept"
        );
        assert!(read_back(&root).contains_key("fresh"));
        let said: Vec<String> = logs.borrow().iter().map(|l| l.msg.clone()).collect();
        assert!(said.iter().any(|m| m.contains("could not be read") && m.contains("main.ron.unreadable")), "{said:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A write that fails leaves the last save whole, and the next flush
    /// writes everything.** The slot stays dirty after a failure; the flush
    /// that follows carries the whole store, not only what changed since.
    #[test]
    fn a_failed_write_keeps_the_old_save_and_the_next_flush_writes_it_all() {
        let root = std::env::temp_dir().join(format!("floptle-savefail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (lua, state) = lua_with_save(&root);
        lua.load("save.set('a', 1) save.flush({ wait = true })").exec().unwrap();
        // The writer's hidden sibling cannot be made: the write fails before
        // the rename, as a full disk would.
        let blocker = root.join("save/.main.ron.tmp");
        std::fs::create_dir_all(&blocker).unwrap();
        let ok: bool = lua.load("save.set('b', 2) return save.flush({ wait = true })").eval().unwrap();
        assert!(!ok, "a failed write reported success");
        assert_eq!(read_back(&root).len(), 1, "the failed write touched the old save");
        assert!(state.borrow().dirty, "a failed write left the slot clean");

        std::fs::remove_dir_all(&blocker).unwrap();
        lua.load("save.set('c', 3)").exec().unwrap();
        flush(&mut state.borrow_mut(), &root, true).unwrap();
        let disk = read_back(&root);
        assert_eq!(disk.len(), 3, "the retry lost a key: {disk:?}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
