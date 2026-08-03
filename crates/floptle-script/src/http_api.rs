//! `http.*` and `json.*` — talking to a web API from Lua.
//!
//! A game that can't reach a server can't have an account, a card list, a
//! leaderboard or a shop. This is the smallest surface that makes those
//! possible without handing scripts a foot-gun.
//!
//! **Non-blocking, always.** A request is handed to a worker thread and the
//! callback runs on a LATER TICK on the main thread, so it is safe to touch
//! nodes from it and a slow server can never stall a frame. There is no
//! blocking form on purpose: the blocking form is the one everybody reaches
//! for, and it turns a 300 ms round trip into a 300 ms freeze.
//!
//! ```lua
//! http.get(url [, opts], function(res) end)
//! http.post(url, body [, opts], function(res) end)
//! -- opts = { headers = {...}, timeout = 10, json = true }
//! -- res  = { ok, status, body, json, error }
//! ```
//!
//! **Outside the fixed tick, deliberately.** A reply arrives when it arrives,
//! which no replay can reproduce — so HTTP lives in the frame pass, and calling
//! it from `fixedUpdate` warns once. It belongs in `update`, `start`, a timer,
//! or an RPC handler.
//!
//! **Play only.** Edit mode never opens a socket: a script being edited must
//! not be able to hit a live endpoint because the Inspector happened to
//! re-run it. Stop cancels everything in flight, and a late reply to a
//! cancelled generation is dropped rather than delivered into a fresh session.
//!
//! **Capped, and it says so.** In-flight and per-second limits, plus a hard
//! body size — a script that calls `http.get` in `update` hits a wall and one
//! Console line explaining it, rather than melting someone's connection in
//! silence.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use mlua::{Function, Lua, Table, Value};

use crate::{LogLevel, ScriptLog};

/// How many requests may be in flight at once. Past this, calls fail fast with
/// `res.error` rather than queueing without bound.
const MAX_IN_FLIGHT: usize = 8;
/// How many may be STARTED per second. A script calling `http.get` every frame
/// is a bug; this is where it finds out.
const MAX_PER_SECOND: usize = 20;
/// Largest response body accepted, in bytes. Past this the request fails with
/// an error instead of buying a script an unbounded allocation.
const MAX_BODY: usize = 8 * 1024 * 1024;
/// Default per-request timeout, seconds.
const DEFAULT_TIMEOUT: f64 = 15.0;

/// What a worker thread sends back. Plain data — no Lua types cross the thread
/// boundary (mlua values are not `Send`, and the callback stays on the main
/// thread where the node handles live).
pub(crate) struct HttpReply {
    id: u64,
    /// The session this belonged to; a reply from an older one is dropped.
    generation: u64,
    status: u16,
    body: String,
    /// The transport/HTTP-level failure, if any.
    error: Option<String>,
    /// Whether the server said the body is JSON.
    said_json: bool,
}

/// One request the main thread is still waiting on.
struct Pending {
    callback: Function,
    /// Parse the body into `res.json` (explicitly asked for, or content-typed).
    want_json: bool,
}

/// The `http.*` bridge: the callbacks waiting, the channel workers reply on,
/// and the rate accounting.
pub(crate) struct HttpState {
    pending: HashMap<u64, Pending>,
    tx: Sender<HttpReply>,
    rx: Receiver<HttpReply>,
    next_id: u64,
    /// Bumped by Stop / scene load — replies stamped with an older one are
    /// dropped, so a request made in the last session cannot fire a callback
    /// into this one.
    generation: u64,
    /// Start times of this second's requests, for the per-second cap.
    recent: Vec<f64>,
    /// The rate/in-flight caps announce themselves ONCE per session. Every
    /// frame would be a Console flood, and never would be a mystery.
    warned_rate: bool,
    warned_fixed: bool,
    /// Wall-clock seconds since the session started, fed by the host.
    now: f64,
    /// Play only — set by the driver. Edit mode never opens a socket.
    playing: bool,
}

impl HttpState {
    pub(crate) fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            pending: HashMap::new(),
            tx,
            rx,
            next_id: 0,
            generation: 0,
            recent: Vec::new(),
            warned_rate: false,
            warned_fixed: false,
            now: 0.0,
            playing: false,
        }
    }

    /// Stop / scene load: forget every callback and disown every reply still on
    /// the wire. The worker threads finish and their results fall on the floor.
    pub(crate) fn cancel_all(&mut self) {
        self.pending.clear();
        self.recent.clear();
        self.generation = self.generation.wrapping_add(1);
        self.warned_rate = false;
        self.warned_fixed = false;
    }

    pub(crate) fn set_playing(&mut self, playing: bool) {
        if self.playing && !playing {
            self.cancel_all();
        }
        self.playing = playing;
    }

    /// How many requests are still waiting (`http.inFlight()`).
    pub(crate) fn in_flight(&self) -> usize {
        self.pending.len()
    }
}

/// Read `opts` into (headers, timeout, explicit-json).
fn read_opts(opts: Option<&Table>) -> (Vec<(String, String)>, f64, Option<bool>) {
    let mut headers = Vec::new();
    let mut timeout = DEFAULT_TIMEOUT;
    let mut want_json = None;
    let Some(t) = opts else { return (headers, timeout, want_json) };
    if let Ok(h) = t.get::<Table>("headers") {
        for pair in h.pairs::<String, Value>().flatten() {
            let v = match pair.1 {
                Value::String(s) => s.to_string_lossy().to_string(),
                Value::Number(n) => crate::api::format_lua_number(n),
                Value::Integer(i) => i.to_string(),
                Value::Boolean(b) => b.to_string(),
                _ => continue,
            };
            headers.push((pair.0, v));
        }
        // Deterministic order: a header map is a hash, and a request that
        // differs run to run is one nobody can reproduce from a log.
        headers.sort();
    }
    if let Ok(n) = t.get::<f64>("timeout") {
        timeout = n.clamp(0.1, 120.0);
    }
    if let Ok(Value::Boolean(b)) = t.get::<Value>("json") {
        want_json = Some(b);
    }
    (headers, timeout, want_json)
}

/// Turn a `serde_json::Value` into a Lua value. Objects become tables, arrays
/// become 1-based arrays, `null` becomes `nil` — which means a null FIELD
/// simply isn't there, the same thing a missing field looks like, and that is
/// the right answer in Lua.
fn json_to_lua(lua: &Lua, v: &serde_json::Value) -> mlua::Result<Value> {
    Ok(match v {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(b) => Value::Boolean(*b),
        serde_json::Value::Number(n) => Value::Number(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(s) => Value::String(lua.create_string(s)?),
        serde_json::Value::Array(a) => {
            let t = lua.create_table()?;
            for (i, x) in a.iter().enumerate() {
                t.raw_set(i + 1, json_to_lua(lua, x)?)?;
            }
            Value::Table(t)
        }
        serde_json::Value::Object(o) => {
            let t = lua.create_table()?;
            for (k, x) in o {
                t.raw_set(k.as_str(), json_to_lua(lua, x)?)?;
            }
            Value::Table(t)
        }
    })
}

/// …and back. A Lua table with a `1` key is an ARRAY; anything else is an
/// object. That is the only rule Lua's one table type can support, and stating
/// it is better than guessing per call.
fn lua_to_json(v: &Value) -> mlua::Result<serde_json::Value> {
    Ok(match v {
        Value::Nil => serde_json::Value::Null,
        Value::Boolean(b) => serde_json::Value::Bool(*b),
        Value::Integer(i) => serde_json::Value::from(*i),
        Value::Number(n) => serde_json::Number::from_f64(*n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::String(s) => serde_json::Value::String(s.to_string_lossy().to_string()),
        Value::Table(t) => {
            if t.raw_len() > 0 || t.contains_key(1)? {
                let mut a = Vec::new();
                for x in t.clone().sequence_values::<Value>() {
                    a.push(lua_to_json(&x?)?);
                }
                serde_json::Value::Array(a)
            } else {
                let mut o = serde_json::Map::new();
                for pair in t.clone().pairs::<Value, Value>() {
                    let (k, x) = pair?;
                    let k = match k {
                        Value::String(s) => s.to_string_lossy().to_string(),
                        Value::Integer(i) => i.to_string(),
                        Value::Number(n) => crate::api::format_lua_number(n),
                        _ => continue,
                    };
                    o.insert(k, lua_to_json(&x)?);
                }
                serde_json::Value::Object(o)
            }
        }
        other => {
            return Err(mlua::Error::RuntimeError(format!(
                "json.encode cannot encode a {}",
                other.type_name()
            )));
        }
    })
}

fn log(logs: &Rc<RefCell<Vec<ScriptLog>>>, level: LogLevel, msg: String) {
    logs.borrow_mut().push(ScriptLog { level, msg, source: None });
}

/// Build the `res` table a callback receives.
fn reply_table(lua: &Lua, r: &HttpReply, want_json: bool) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let ok = r.error.is_none() && (200..300).contains(&r.status);
    t.set("ok", ok)?;
    t.set("status", r.status)?;
    t.set("body", r.body.as_str())?;
    if let Some(e) = &r.error {
        t.set("error", e.as_str())?;
    }
    if want_json || r.said_json {
        match serde_json::from_str::<serde_json::Value>(&r.body) {
            Ok(v) => t.set("json", json_to_lua(lua, &v)?)?,
            // Malformed JSON SETS res.error rather than raising: a server
            // having a bad day must not take a script down with it.
            Err(e) if r.error.is_none() => {
                t.set("ok", false)?;
                t.set("error", format!("the reply is not valid JSON: {e}"))?;
            }
            Err(_) => {}
        }
    }
    Ok(t)
}

/// Deliver every reply that has arrived. Called from the host's FRAME pass —
/// never the tick pass, because a reply's arrival time is not reproducible and
/// a replay must never see one.
pub(crate) fn drain(
    lua: &Lua,
    state: &Rc<RefCell<HttpState>>,
    logs: &Rc<RefCell<Vec<ScriptLog>>>,
) {
    // Collect with the borrow held; call with it RELEASED — a callback that
    // fires another request re-borrows the state.
    let ready: Vec<(HttpReply, Pending)> = {
        let mut s = state.borrow_mut();
        let session = s.generation;
        let mut out = Vec::new();
        while let Ok(r) = s.rx.try_recv() {
            if r.generation != session {
                continue; // a reply to the previous session
            }
            if let Some(p) = s.pending.remove(&r.id) {
                out.push((r, p));
            }
        }
        out
    };
    for (r, p) in ready {
        match reply_table(lua, &r, p.want_json) {
            Ok(t) => {
                if let Err(e) = p.callback.call::<()>(t) {
                    log(logs, LogLevel::Error, format!("http callback: {e}"));
                }
            }
            Err(e) => log(logs, LogLevel::Error, format!("http reply: {e}")),
        }
    }
}

/// Feed the wall clock (seconds since the session started) — the per-second
/// cap's time base.
pub(crate) fn set_now(state: &Rc<RefCell<HttpState>>, now: f64) {
    state.borrow_mut().now = now;
}

/// Start one request on a worker thread.
#[allow(clippy::too_many_arguments)]
fn send(
    state: &Rc<RefCell<HttpState>>,
    logs: &Rc<RefCell<Vec<ScriptLog>>>,
    in_fixed: &Rc<Cell<bool>>,
    method: &'static str,
    url: String,
    body: Option<String>,
    headers: Vec<(String, String)>,
    timeout: f64,
    want_json: bool,
    callback: Function,
) -> mlua::Result<()> {
    let mut s = state.borrow_mut();
    if !s.playing {
        return Err(mlua::Error::RuntimeError(
            "http is Play-only — edit mode never opens a socket (a script being edited must \
             not be able to hit a live endpoint because the Inspector re-ran it)"
                .into(),
        ));
    }
    if url.len() > 2048 {
        return Err(mlua::Error::RuntimeError("that URL is absurdly long".into()));
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(mlua::Error::RuntimeError(format!(
            "http.{}: the URL must start with http:// or https:// (got '{url}')",
            method.to_ascii_lowercase()
        )));
    }
    // A call from fixedUpdate warns ONCE. It is not an error — the request will
    // work — but it can never be replayed, so a rollback match that depends on
    // it will diverge, and that is worth saying out loud exactly one time.
    if in_fixed.get() && !s.warned_fixed {
        s.warned_fixed = true;
        drop(s);
        log(
            logs,
            LogLevel::Warn,
            "http called from fixedUpdate: a reply arrives when it arrives, so no replay can \
             reproduce it and a rollback match will diverge. Move the call to update, start, \
             a timer, or an RPC handler."
                .into(),
        );
        s = state.borrow_mut();
    }
    let now = s.now;
    s.recent.retain(|t| now - *t < 1.0);
    if s.pending.len() >= MAX_IN_FLIGHT || s.recent.len() >= MAX_PER_SECOND {
        let which = if s.pending.len() >= MAX_IN_FLIGHT {
            format!("{MAX_IN_FLIGHT} requests already in flight")
        } else {
            format!("{MAX_PER_SECOND} requests started in the last second")
        };
        if !s.warned_rate {
            s.warned_rate = true;
            drop(s);
            log(
                logs,
                LogLevel::Warn,
                format!(
                    "http rate limit: {which}. Further calls fail fast until it clears. This \
                     is nearly always a request inside update() — move it to start(), a timer, \
                     or fire it once and keep the answer."
                ),
            );
        }
        return Err(mlua::Error::RuntimeError(format!("http rate limit: {which}")));
    }
    let id = s.next_id;
    s.next_id += 1;
    s.recent.push(now);
    let generation = s.generation;
    let tx = s.tx.clone();
    s.pending.insert(id, Pending { callback, want_json });
    drop(s);

    // One thread per request, bounded by MAX_IN_FLIGHT above. A pool would save
    // a few hundred microseconds of spawn and cost a lifetime of shutdown
    // bookkeeping; at eight concurrent requests this is the right trade.
    let agent = Arc::new(
        ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs_f64(timeout))
            .build(),
    );
    std::thread::Builder::new()
        .name("floptle-http".into())
        .spawn(move || {
            let mut req = match method {
                "POST" => agent.post(&url),
                "PUT" => agent.put(&url),
                "DELETE" => agent.delete(&url),
                _ => agent.get(&url),
            };
            for (k, v) in &headers {
                req = req.set(k, v);
            }
            let res = match body {
                Some(b) => req.send_string(&b),
                None => req.call(),
            };
            let reply = match res {
                // ureq treats 4xx/5xx as Err(Status) — the body is still there,
                // and a script wants it: an API's error message lives in it.
                Ok(r) | Err(ureq::Error::Status(_, r)) => {
                    let status = r.status();
                    let said_json = r
                        .header("content-type")
                        .is_some_and(|c| c.to_ascii_lowercase().contains("json"));
                    use std::io::Read as _;
                    let mut buf = String::new();
                    let read =
                        r.into_reader().take(MAX_BODY as u64 + 1).read_to_string(&mut buf);
                    let error = match read {
                        Err(e) => Some(format!("could not read the reply: {e}")),
                        Ok(_) if buf.len() > MAX_BODY => {
                            buf.clear();
                            Some(format!("the reply is larger than the {MAX_BODY} byte limit"))
                        }
                        Ok(_) => None,
                    };
                    HttpReply { id, generation, status, body: buf, error, said_json }
                }
                Err(e) => HttpReply {
                    id,
                    generation,
                    status: 0,
                    body: String::new(),
                    error: Some(e.to_string()),
                    said_json: false,
                },
            };
            // The receiver is gone only when the host itself has: nothing to do.
            let _ = tx.send(reply);
        })
        .map_err(|e| mlua::Error::RuntimeError(format!("http: could not start a worker: {e}")))?;
    Ok(())
}

/// Sort `http.get(url [, opts], fn)` / `http.post(url, body [, opts], fn)` out
/// of the argument list. The optional middle argument is what makes this
/// fiddly and what makes the call read well, so it is worth doing once.
fn parse_args(
    args: Vec<Value>,
    has_body: bool,
) -> mlua::Result<(String, Option<String>, Option<Table>, Function)> {
    let mut it = args.into_iter();
    let url = match it.next() {
        Some(Value::String(s)) => s.to_string_lossy().to_string(),
        _ => return Err(mlua::Error::RuntimeError("the first argument is a URL string".into())),
    };
    let body = if has_body {
        match it.next() {
            Some(Value::String(s)) => Some(s.to_string_lossy().to_string()),
            Some(Value::Nil) | None => Some(String::new()),
            // A table body is JSON — the overwhelmingly common case, and
            // making people call json.encode by hand for it is friction with
            // no lesson in it.
            Some(Value::Table(t)) => Some(
                serde_json::to_string(&lua_to_json(&Value::Table(t))?)
                    .map_err(|e| mlua::Error::RuntimeError(format!("encoding the body: {e}")))?,
            ),
            Some(other) => {
                return Err(mlua::Error::RuntimeError(format!(
                    "the body is a string or a table, not a {}",
                    other.type_name()
                )));
            }
        }
    } else {
        None
    };
    let rest: Vec<Value> = it.collect();
    let (opts, cb) = match rest.len() {
        1 => (None, rest.into_iter().next()),
        2 => {
            let mut r = rest.into_iter();
            let o = match r.next() {
                Some(Value::Table(t)) => Some(t),
                Some(Value::Nil) | None => None,
                Some(other) => {
                    return Err(mlua::Error::RuntimeError(format!(
                        "opts is a table like {{ headers = {{}}, timeout = 10 }}, not a {}",
                        other.type_name()
                    )));
                }
            };
            (o, r.next())
        }
        _ => (None, None),
    };
    match cb {
        Some(Value::Function(f)) => Ok((url, body, opts, f)),
        _ => Err(mlua::Error::RuntimeError(
            "the last argument is the callback: function(res) ... end".into(),
        )),
    }
}

/// Install `http.*`, `json.*` and `openUrl`.
pub(crate) fn install_http_api(
    lua: &Lua,
    state: Rc<RefCell<HttpState>>,
    logs: Rc<RefCell<Vec<ScriptLog>>>,
    in_fixed: Rc<Cell<bool>>,
) {
    // ---- json ---------------------------------------------------------------
    if let Ok(t) = lua.create_table() {
        if let Ok(f) = lua.create_function(|_, v: Value| {
            let j = lua_to_json(&v)?;
            serde_json::to_string(&j)
                .map_err(|e| mlua::Error::RuntimeError(format!("json.encode: {e}")))
        }) {
            let _ = t.set("encode", f);
        }
        // decode returns `nil, message` on bad input rather than raising: a
        // reply from someone else's server is data, not a programming error,
        // and `local v, why = json.decode(s)` is the honest shape for it.
        if let Ok(f) = lua.create_function(|lua, s: String| {
            match serde_json::from_str::<serde_json::Value>(&s) {
                Ok(v) => Ok((json_to_lua(lua, &v)?, Value::Nil)),
                Err(e) => Ok((Value::Nil, Value::String(lua.create_string(e.to_string())?))),
            }
        }) {
            let _ = t.set("decode", f);
        }
        let _ = lua.globals().set("json", t);
    }

    // ---- http ---------------------------------------------------------------
    let Ok(t) = lua.create_table() else { return };
    for (name, method, has_body) in [
        ("get", "GET", false),
        ("post", "POST", true),
        ("put", "PUT", true),
        ("delete", "DELETE", false),
    ] {
        let st = state.clone();
        let lg = logs.clone();
        let fx = in_fixed.clone();
        if let Ok(f) = lua.create_function(move |_, args: mlua::MultiValue| {
            let (url, body, opts, cb) = parse_args(args.into_iter().collect(), has_body)?;
            let (headers, timeout, explicit_json) = read_opts(opts.as_ref());
            // `json = true` in opts forces a parse; otherwise the content-type
            // decides, which is what people expect and never surprises anyone.
            let want_json = explicit_json.unwrap_or(false);
            send(&st, &lg, &fx, method, url, body, headers, timeout, want_json, cb)
        }) {
            let _ = t.set(name, f);
        }
    }
    {
        let st = state.clone();
        if let Ok(f) = lua.create_function(move |_, ()| Ok(st.borrow().in_flight())) {
            let _ = t.set("inFlight", f);
        }
    }
    {
        let st = state.clone();
        if let Ok(f) = lua.create_function(move |_, ()| {
            st.borrow_mut().cancel_all();
            Ok(())
        }) {
            let _ = t.set("cancelAll", f);
        }
    }
    let _ = lua.globals().set("http", t);

    // ---- openUrl ------------------------------------------------------------
    // Needed by the device-code login flow: the player approves the pairing in
    // a real browser, so the game never sees a password. Play-only for the same
    // reason http is — an editing session must not be able to open tabs.
    {
        let st = state.clone();
        let lg = logs.clone();
        if let Ok(f) = lua.create_function(move |_, url: String| {
            if !st.borrow().playing {
                return Err(mlua::Error::RuntimeError("openUrl is Play-only".into()));
            }
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(mlua::Error::RuntimeError(
                    "openUrl takes an http:// or https:// address".into(),
                ));
            }
            match open_in_browser(&url) {
                Ok(()) => Ok(()),
                Err(e) => {
                    // Not an error worth killing a script over: print the URL so
                    // the player can still get there by hand.
                    log(&lg, LogLevel::Warn, format!("openUrl: {e} — open it yourself: {url}"));
                    Ok(())
                }
            }
        }) {
            let _ = lua.globals().set("openUrl", f);
        }
    }
}

/// Hand a URL to the platform's browser. The Hub does the same for its own
/// hyperlinks; this is the one call, kept here so a script can use it.
fn open_in_browser(url: &str) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    let cmd = ("xdg-open", vec![url]);
    #[cfg(target_os = "macos")]
    let cmd = ("open", vec![url]);
    #[cfg(target_os = "windows")]
    let cmd = ("cmd", vec!["/C", "start", "", url]);
    std::process::Command::new(cmd.0)
        .args(cmd.1)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Lua state with `http`/`json` installed, plus the two handles a test
    /// pokes at: the request bookkeeping and the Console.
    type Harness = (Lua, Rc<RefCell<HttpState>>, Rc<RefCell<Vec<ScriptLog>>>);

    fn lua_with_http() -> Harness {
        let lua = Lua::new();
        let state = Rc::new(RefCell::new(HttpState::new()));
        let logs: Rc<RefCell<Vec<ScriptLog>>> = Rc::new(RefCell::new(Vec::new()));
        install_http_api(&lua, state.clone(), logs.clone(), Rc::new(Cell::new(false)));
        (lua, state, logs)
    }

    /// `json.encode` / `json.decode` round-trip the shapes a web API actually
    /// sends, and `decode` reports bad input as a VALUE rather than raising —
    /// someone else's server having a bad day is data, not a bug in your script.
    #[test]
    fn json_round_trips_and_reports_bad_input_without_raising() {
        let (lua, _, _) = lua_with_http();
        let s = |src: &str| -> String { lua.load(src).call::<String>(()).expect(src) };
        let n = |src: &str| -> f64 { lua.load(src).call::<f64>(()).expect(src) };

        assert_eq!(n("return json.decode('{\"hp\": 42}').hp"), 42.0);
        assert_eq!(s("return json.decode('{\"n\": \"ok\"}').n"), "ok");
        assert_eq!(n("return #json.decode('[1,2,3]')"), 3.0);
        assert_eq!(n("return json.decode('[[1,2],[3]]')[1][2]"), 2.0);
        // Nested objects and booleans survive the trip both ways.
        assert_eq!(
            n("local t = json.decode(json.encode{ a = { b = { c = 7 } } }) return t.a.b.c"),
            7.0
        );
        assert!(lua
            .load("return json.decode(json.encode{ on = true }).on")
            .call::<bool>(())
            .unwrap());
        // A list encodes as an ARRAY, not an object with numeric keys.
        assert_eq!(s("return json.encode{1, 2, 3}"), "[1,2,3]");
        assert_eq!(s("return json.encode{}"), "{}");
        // Bad input: nil + a message, never an error.
        let (v, why): (Value, Value) =
            lua.load("return json.decode('{oh no')").call(()).unwrap();
        assert!(matches!(v, Value::Nil), "bad JSON must decode to nil");
        assert!(matches!(why, Value::String(_)), "…and say why");
        // A value json can't represent is refused with a message that names it.
        let e = lua.load("return json.encode(print)").exec().unwrap_err().to_string();
        assert!(e.contains("cannot encode"), "{e}");
    }

    /// Nothing opens a socket outside Play, and the refusal says why. An
    /// editing session must not be able to hit a live endpoint because the
    /// Inspector happened to re-run a script.
    #[test]
    fn http_refuses_to_do_anything_in_edit_mode() {
        let (lua, state, _) = lua_with_http();
        let e = lua
            .load("http.get('https://example.com', function(r) end)")
            .exec()
            .unwrap_err()
            .to_string();
        assert!(e.contains("Play-only"), "{e}");
        let e = lua.load("openUrl('https://example.com')").exec().unwrap_err().to_string();
        assert!(e.contains("Play-only"), "{e}");
        // In Play the argument checks are what refuse, not the mode check —
        // and no request is booked for a call that never made sense.
        state.borrow_mut().set_playing(true);
        for (src, want) in [
            ("http.get('ftp://nope', function() end)", "http://"),
            ("http.get('https://x.com')", "callback"),
            ("http.get(42, function() end)", "URL string"),
            ("http.post('https://x.com', print, function() end)", "body is a string or a table"),
            ("http.get('https://x.com', 7, function() end)", "opts is a table"),
        ] {
            let e = lua.load(src).exec().unwrap_err().to_string();
            assert!(e.contains(want), "{src}\nexpected {want:?}, got: {e}");
        }
        assert_eq!(state.borrow().in_flight(), 0, "a refused call must book nothing");
    }

    /// The caps hold, and they announce themselves exactly once.
    ///
    /// Silence would leave a script mysteriously half-working; a line per
    /// refusal would bury the Console under the very loop that caused it.
    #[test]
    fn the_rate_cap_holds_and_says_so_exactly_once() {
        let (lua, state, logs) = lua_with_http();
        state.borrow_mut().set_playing(true);
        // Fill the in-flight table directly — no sockets in a unit test.
        {
            let mut s = state.borrow_mut();
            let cb: Function = lua.load("return function() end").eval().unwrap();
            for i in 0..MAX_IN_FLIGHT {
                s.pending.insert(i as u64, Pending { callback: cb.clone(), want_json: false });
            }
        }
        for _ in 0..5 {
            let e = lua
                .load("http.get('https://example.com', function() end)")
                .exec()
                .unwrap_err()
                .to_string();
            assert!(e.contains("rate limit"), "{e}");
        }
        let warns = logs
            .borrow()
            .iter()
            .filter(|l| l.msg.contains("rate limit") && l.level == LogLevel::Warn)
            .count();
        assert_eq!(warns, 1, "the cap must announce itself once, not never and not five times");
    }

    /// Stop cancels everything in flight, and a reply from the old session is
    /// dropped instead of firing a callback into the new one.
    ///
    /// That callback closes over nodes from a scene that no longer exists —
    /// delivering it is how a fresh Play inherits the last one's network.
    #[test]
    fn a_reply_from_the_previous_session_is_never_delivered() {
        let (lua, state, logs) = lua_with_http();
        state.borrow_mut().set_playing(true);
        let fired: Rc<Cell<u32>> = Rc::new(Cell::new(0));
        {
            let f = fired.clone();
            let cb = lua
                .create_function(move |_, _t: Table| {
                    f.set(f.get() + 1);
                    Ok(())
                })
                .unwrap();
            let mut s = state.borrow_mut();
            let (id, session) = (7u64, s.generation);
            s.pending.insert(id, Pending { callback: cb, want_json: false });
            // A reply that arrives before anything is cancelled: delivered.
            s.tx.send(HttpReply {
                id,
                generation: session,
                status: 200,
                body: "{}".into(),
                error: None,
                said_json: false,
            })
            .unwrap();
        }
        drain(&lua, &state, &logs);
        assert_eq!(fired.get(), 1, "a live reply must reach its callback");

        // Now Stop, then a reply stamped with the OLD generation arrives.
        let stale_gen = state.borrow().generation;
        {
            let f = fired.clone();
            let cb = lua
                .create_function(move |_, _t: Table| {
                    f.set(f.get() + 100);
                    Ok(())
                })
                .unwrap();
            state.borrow_mut().pending.insert(9, Pending { callback: cb, want_json: false });
        }
        state.borrow_mut().set_playing(false); // Stop
        assert_eq!(state.borrow().in_flight(), 0, "Stop forgets every pending callback");
        let s = state.borrow();
        s.tx.send(HttpReply {
            id: 9,
            generation: stale_gen,
            status: 200,
            body: "{}".into(),
            error: None,
            said_json: false,
        })
        .unwrap();
        drop(s);
        drain(&lua, &state, &logs);
        assert_eq!(fired.get(), 1, "a reply to the previous session must fall on the floor");
    }

    /// The `res` table: what `ok` means, that a 404's BODY still arrives (an
    /// API's error message lives in it), and that malformed JSON sets
    /// `res.error` instead of raising inside the callback.
    #[test]
    fn the_reply_table_says_what_happened() {
        let (lua, _, _) = lua_with_http();
        let mk = |status: u16, body: &str, said_json: bool, want_json: bool| {
            let r = HttpReply {
                id: 0,
                generation: 0,
                status,
                body: body.into(),
                error: None,
                said_json,
            };
            reply_table(&lua, &r, want_json).unwrap()
        };
        let t = mk(200, r#"{"token":"abc"}"#, true, false);
        assert!(t.get::<bool>("ok").unwrap());
        assert_eq!(t.get::<Table>("json").unwrap().get::<String>("token").unwrap(), "abc");
        // A 404 is not ok — but the body is there, because that is where the
        // server explains itself.
        let t = mk(404, r#"{"error":"no such card"}"#, true, false);
        assert!(!t.get::<bool>("ok").unwrap());
        assert_eq!(t.get::<u16>("status").unwrap(), 404);
        assert_eq!(
            t.get::<Table>("json").unwrap().get::<String>("error").unwrap(),
            "no such card"
        );
        // Content-type said JSON and it isn't: error, not a raise.
        let t = mk(200, "<html>nope</html>", true, false);
        assert!(!t.get::<bool>("ok").unwrap());
        assert!(t.get::<String>("error").unwrap().contains("not valid JSON"));
        // No JSON asked for and none advertised: body only, no parse attempted.
        let t = mk(200, "plain text", false, false);
        assert!(t.get::<bool>("ok").unwrap());
        assert!(matches!(t.get::<Value>("json").unwrap(), Value::Nil));
        assert_eq!(t.get::<String>("body").unwrap(), "plain text");
    }
}
