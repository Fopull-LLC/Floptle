//! `assets.preload` — models imported ahead of the moment a game needs them.
//!
//! ```lua
//! assets.preload({ "models/arm.glb", "models/leg.glb" }, function(failed)
//!   ready = true            -- `failed` lists any path that could not load
//! end)
//! ```
//!
//! A model a node is given mid-game loads in the background and draws once it
//! is in, so nothing freezes; but it is not there on the frame it was asked
//! for. A game that needs it on that frame (a ragdoll whose parts must appear
//! the moment the enemy dies) asks for it earlier, on a menu or a loading
//! screen, and is told when it is ready.
//!
//! The script host only keeps the book. The editor starts the imports, and
//! reports which paths are done; callbacks run in the frame pass, like every
//! other reply a script waits for.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use mlua::{Function, Lua, RegistryKey, Value};

use crate::{LogLevel, ScriptLog};

/// What scripts asked to preload, and who is waiting.
#[derive(Default)]
pub(crate) struct Preloads {
    /// Paths nobody has started importing yet.
    requested: Vec<String>,
    /// One per call: the paths it named, and its callback.
    waiting: Vec<(Vec<String>, Option<RegistryKey>)>,
    /// The editor's latest answer per waited-on path: loaded or failed.
    status: HashMap<String, bool>,
}

impl Preloads {
    pub(crate) fn take_requests(&mut self) -> Vec<String> {
        std::mem::take(&mut self.requested)
    }

    /// Every path a callback is still waiting on.
    pub(crate) fn waiting_on(&self) -> Vec<String> {
        let mut v: Vec<String> = self.waiting.iter().flat_map(|(p, _)| p.iter().cloned()).collect();
        v.sort();
        v.dedup();
        v
    }

    pub(crate) fn set_status(&mut self, status: HashMap<String, bool>) {
        self.status = status;
    }

    /// Stop / scene load: a callback from the last session closes over nodes
    /// that are gone. The imports themselves carry on; the models stay loaded.
    pub(crate) fn cancel_all(&mut self) {
        self.waiting.clear();
        self.requested.clear();
    }
}

pub(crate) fn install(lua: &Lua, assets: &mlua::Table, state: Rc<RefCell<Preloads>>) -> mlua::Result<()> {
    let f = lua.create_function(move |lua, (list, cb): (Value, Option<Function>)| {
        let paths: Vec<String> = match list {
            Value::String(s) => vec![s.to_str()?.to_string()],
            Value::Table(t) => t.sequence_values::<String>().collect::<mlua::Result<_>>().map_err(|_| {
                mlua::Error::runtime("assets.preload takes a list of model paths (strings)")
            })?,
            _ => {
                return Err(mlua::Error::runtime(
                    "assets.preload takes a model path or a list of them, then an optional callback",
                ));
            }
        };
        let key = cb.map(|f| lua.create_registry_value(f)).transpose()?;
        let mut s = state.borrow_mut();
        s.requested.extend(paths.iter().cloned());
        s.waiting.push((paths, key));
        Ok(())
    })?;
    assets.set("preload", f)
}

/// Run every callback whose paths are all in: frame pass only.
pub(crate) fn drain(lua: &Lua, state: &Rc<RefCell<Preloads>>, logs: &Rc<RefCell<Vec<ScriptLog>>>) {
    // Collect with the borrow held, call with it released: a callback that
    // preloads again re-borrows the state.
    let ready: Vec<(Vec<String>, Option<RegistryKey>)> = {
        let Ok(mut s) = state.try_borrow_mut() else { return };
        let status = std::mem::take(&mut s.status);
        let (done, still): (Vec<_>, Vec<_>) =
            std::mem::take(&mut s.waiting).into_iter().partition(|(paths, _)| {
                paths.iter().all(|p| status.contains_key(p))
            });
        s.waiting = still;
        s.status = status.clone();
        done.into_iter()
            .map(|(paths, key)| (paths.into_iter().filter(|p| status.get(p) == Some(&false)).collect(), key))
            .collect()
    };
    for (failed, key) in ready {
        let Some(key) = key else { continue };
        let Ok(cb) = lua.registry_value::<Function>(&key) else { continue };
        let _ = lua.remove_registry_value(key);
        let list = match lua.create_sequence_from(failed) {
            Ok(t) => t,
            Err(e) => {
                logs.borrow_mut().push(ScriptLog { level: LogLevel::Error, msg: format!("assets.preload: {e}"), source: None });
                continue;
            }
        };
        if let Err(e) = cb.call::<()>(list) {
            logs.borrow_mut().push(ScriptLog {
                level: LogLevel::Error,
                msg: format!("assets.preload callback: {e}"),
                source: None,
            });
        }
    }
}
