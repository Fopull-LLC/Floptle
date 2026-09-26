//! `assets.preload` — models and sounds loaded ahead of the moment a game
//! needs them.
//!
//! ```lua
//! assets.preload({ "models/arm.glb", "models/leg.glb", "audio/hit.wav" }, function(failed)
//!   ready = true            -- `failed` lists any path that could not load
//! end)
//! audio.preload("audio/hit")  -- a sound, named the way audio.play names it
//! ```
//!
//! `assets.preload` tells a sound from a model by its extension; `audio.preload`
//! takes the extensionless names `audio.play` accepts.
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

/// What a preloaded path is, which decides who loads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreloadKind {
    Model,
    Sound,
}

/// One preload call: the paths it named, each with its kind, and its callback.
type Wait = (Vec<(String, PreloadKind)>, Option<RegistryKey>);

/// What scripts asked to preload, and who is waiting.
#[derive(Default)]
pub(crate) struct Preloads {
    /// Paths nobody has started loading yet.
    requested: Vec<(String, PreloadKind)>,
    /// One per call: the paths it named, and its callback.
    waiting: Vec<Wait>,
    /// The editor's latest answer per waited-on path: loaded or failed.
    status: HashMap<String, bool>,
}

impl Preloads {
    pub(crate) fn take_requests(&mut self, kind: PreloadKind) -> Vec<String> {
        let (take, keep) = std::mem::take(&mut self.requested).into_iter().partition(|(_, k)| *k == kind);
        self.requested = keep;
        take.into_iter().map(|(p, _)| p).collect()
    }

    /// Every path of this kind a callback is still waiting on.
    pub(crate) fn waiting_on(&self, kind: PreloadKind) -> Vec<String> {
        let mut v: Vec<String> = self
            .waiting
            .iter()
            .flat_map(|(p, _)| p.iter().filter(|(_, k)| *k == kind).map(|(p, _)| p.clone()))
            .collect();
        v.sort();
        v.dedup();
        v
    }

    /// The editor's answers for one kind. The other kind's answers stand for
    /// as long as something still waits on them; nothing else is kept, so an
    /// old answer cannot settle a later preload of the same path.
    pub(crate) fn set_status(&mut self, kind: PreloadKind, status: HashMap<String, bool>) {
        let other = match kind {
            PreloadKind::Model => PreloadKind::Sound,
            PreloadKind::Sound => PreloadKind::Model,
        };
        let keep = self.waiting_on(other);
        self.status.retain(|p, _| keep.contains(p));
        self.status.extend(status);
    }

    /// Stop / scene load: a callback from the last session closes over nodes
    /// that are gone. The imports themselves carry on; the models stay loaded.
    pub(crate) fn cancel_all(&mut self) {
        self.waiting.clear();
        self.requested.clear();
    }
}

/// `assets.preload(paths, cb)`: models, and sounds by their extension.
pub(crate) fn install(lua: &Lua, assets: &mlua::Table, state: Rc<RefCell<Preloads>>) -> mlua::Result<()> {
    assets.set("preload", preload_fn(lua, state, "assets.preload", "asset", |p| {
        if floptle_audio::is_audio_path(std::path::Path::new(p)) { PreloadKind::Sound } else { PreloadKind::Model }
    })?)
}

/// `audio.preload(clips, cb)`: every name is a sound, spelled as `audio.play`
/// takes it. Installed on the `audio` table once that exists.
pub(crate) fn install_audio(lua: &Lua, state: Rc<RefCell<Preloads>>) -> mlua::Result<()> {
    let audio: mlua::Table = lua.globals().get("audio")?;
    audio.set("preload", preload_fn(lua, state, "audio.preload", "clip", |_| PreloadKind::Sound)?)
}

fn preload_fn(
    lua: &Lua,
    state: Rc<RefCell<Preloads>>,
    call: &'static str,
    what: &'static str,
    kind_of: fn(&str) -> PreloadKind,
) -> mlua::Result<Function> {
    lua.create_function(move |lua, (list, cb): (Value, Option<Function>)| {
        let paths: Vec<String> = match list {
            Value::String(s) => vec![s.to_str()?.to_string()],
            Value::Table(t) => t
                .sequence_values::<String>()
                .collect::<mlua::Result<_>>()
                .map_err(|_| mlua::Error::runtime(format!("{call} takes a list of {what} paths (strings)")))?,
            _ => {
                return Err(mlua::Error::runtime(format!(
                    "{call} takes a {what} path or a list of them, then an optional callback"
                )));
            }
        };
        let paths: Vec<(String, PreloadKind)> = paths.into_iter().map(|p| {
            let k = kind_of(&p);
            (p, k)
        }).collect();
        let key = cb.map(|f| lua.create_registry_value(f)).transpose()?;
        let mut s = state.borrow_mut();
        s.requested.extend(paths.iter().cloned());
        s.waiting.push((paths, key));
        Ok(())
    })
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
                paths.iter().all(|(p, _)| status.contains_key(p))
            });
        s.waiting = still;
        s.status = status.clone();
        done.into_iter()
            .map(|(paths, key)| {
                (paths.into_iter().map(|(p, _)| p).filter(|p| status.get(p) == Some(&false)).collect(), key)
            })
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
