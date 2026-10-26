//! Files the player hands a game: an emblem, an avatar, a shared save, a mod.
//!
//! ```lua
//! app.openFile({ title = "Agency emblem", filter = { "png", "jpg" }, maxBytes = 8e6 },
//!              function(name, bytes, err) end)
//! function filesDropped(node, files) end   -- files[i] = { name = …, bytes = … }
//! ```
//!
//! The game gets the file's name and its bytes, never a path: what the player
//! picked or dropped is all it can read, and it cannot learn where anything
//! lives on their machine. `bytes` is a Lua string, ready for
//! `assets.textureFromBytes` or `assets.writeBytes` to `user://`.
//!
//! The book is kept here; the driver shows the system's picker off the frame,
//! so the game keeps drawing while it is open, reads what was picked, and
//! answers on a later frame. One picker at a time. A host with no picker (a
//! dedicated server, a browser for now) answers `nil, nil, "unsupported"`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use mlua::{Function, Lua, Table, Value};

use crate::{LogLevel, ScriptLog};

/// The size a file may be when a game names none: what
/// `assets.textureFromBytes` takes.
pub const DEFAULT_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// The most any one file handed to a game may be, whatever it asks.
pub const MAX_BYTES: u64 = 64 * 1024 * 1024;

const KEYS: &[&str] = &["title", "filter", "maxBytes"];

/// One `app.openFile` a script made.
#[derive(Debug, Clone)]
pub struct OpenFileRequest {
    pub id: u64,
    pub title: String,
    /// Extensions without the dot; empty for any file.
    pub filter: Vec<String>,
    pub max_bytes: u64,
}

/// What a picker came back with: the file's name and bytes, or why not
/// (`cancelled`, `too-large`, `unsupported`, `busy`, or a read error).
pub type OpenFileAnswer = Result<(String, Vec<u8>), String>;

#[derive(Default)]
pub(crate) struct Files {
    next: u64,
    requests: Vec<OpenFileRequest>,
    waiting: HashMap<u64, Function>,
    answers: Vec<(u64, OpenFileAnswer)>,
}

impl Files {
    pub(crate) fn take_requests(&mut self) -> Vec<OpenFileRequest> {
        std::mem::take(&mut self.requests)
    }

    pub(crate) fn answer(&mut self, id: u64, result: OpenFileAnswer) {
        self.answers.push((id, result));
    }

    /// Scene load or Stop: the callbacks close over nodes that are gone. A
    /// picker still open answers nobody.
    pub(crate) fn cancel_all(&mut self) {
        self.waiting.clear();
        self.requests.clear();
        self.answers.clear();
    }
}

fn parse_filter(opts: &Table) -> mlua::Result<Vec<String>> {
    match opts.get::<Value>("filter")? {
        Value::Nil => Ok(Vec::new()),
        Value::String(s) => Ok(vec![clean_ext(&s.to_string_lossy())]),
        Value::Table(t) => {
            let mut out = Vec::new();
            for v in t.sequence_values::<String>() {
                out.push(clean_ext(&v?));
            }
            Ok(out)
        }
        other => Err(mlua::Error::RuntimeError(format!(
            "app.openFile: filter is a list of extensions like {{ \"png\", \"jpg\" }} (got a {})",
            other.type_name()
        ))),
    }
}

fn clean_ext(e: &str) -> String {
    e.trim().trim_start_matches("*.").trim_start_matches('.').to_ascii_lowercase()
}

pub(crate) fn install(lua: &Lua, state: Rc<RefCell<Files>>) -> mlua::Result<()> {
    let app: Table = lua.globals().get("app")?;
    app.set(
        "openFile",
        lua.create_function(move |lua, (opts, cb): (Option<Table>, Function)| {
            let opts = match opts {
                Some(t) => t,
                None => lua.create_table()?,
            };
            crate::opts::check_keys(&opts, KEYS, "app.openFile")?;
            let title = opts.get::<Option<String>>("title")?.unwrap_or_else(|| "Open".to_string());
            let filter = parse_filter(&opts)?;
            let max_bytes = match opts.get::<Option<f64>>("maxBytes")? {
                Some(b) if b > 0.0 => (b as u64).min(MAX_BYTES),
                Some(_) => {
                    return Err(mlua::Error::RuntimeError("app.openFile: maxBytes must be above 0".into()));
                }
                None => DEFAULT_MAX_BYTES,
            };
            let mut s = state.borrow_mut();
            s.next += 1;
            let id = s.next;
            s.waiting.insert(id, cb);
            // One picker at a time: a second while one is open would stack
            // windows the player has to close in the right order.
            if s.waiting.len() > 1 {
                s.answers.push((id, Err("busy".into())));
                return Ok(());
            }
            s.requests.push(OpenFileRequest { id, title, filter, max_bytes });
            Ok(())
        })?,
    )?;
    Ok(())
}

/// Call back every answered picker: `cb(name, bytes, nil)` or
/// `cb(nil, nil, why)`.
pub(crate) fn drain(lua: &Lua, state: &Rc<RefCell<Files>>, logs: &Rc<RefCell<Vec<ScriptLog>>>) {
    let ready: Vec<(Function, OpenFileAnswer)> = {
        let Ok(mut s) = state.try_borrow_mut() else { return };
        let answers = std::mem::take(&mut s.answers);
        answers.into_iter().filter_map(|(id, r)| s.waiting.remove(&id).map(|cb| (cb, r))).collect()
    };
    for (cb, result) in ready {
        let called = match result {
            Ok((name, bytes)) => match (lua.create_string(&name), lua.create_string(&bytes)) {
                (Ok(n), Ok(b)) => cb.call::<()>((n, b, Value::Nil)),
                (Err(e), _) | (_, Err(e)) => Err(e),
            },
            Err(why) => match lua.create_string(&why) {
                Ok(w) => cb.call::<()>((Value::Nil, Value::Nil, w)),
                Err(e) => Err(e),
            },
        };
        if let Err(e) = called {
            logs.borrow_mut().push(ScriptLog {
                level: LogLevel::Error,
                msg: format!("app.openFile callback: {e}"),
                source: None,
            });
        }
    }
}
