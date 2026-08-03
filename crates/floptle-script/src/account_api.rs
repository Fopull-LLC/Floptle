//! `account.*` — the player's Foverse account, from Lua.
//!
//! ```lua
//! account.signIn()                       -- begins; returns immediately
//! account.state()                        -- "signedOut" | "starting" | "waiting" | "signedIn" | "failed"
//! account.code()                         -- while waiting: { code = "WXYZ-9999", url = "…" }
//! account.player()                       -- when signed in: { id, name, email, tier }
//! account.get("/wallet", function(res) end)
//! account.post("/games/fofighter/events", { event = "cpu_match_won" }, function(res) end)
//! ```
//!
//! **A script asks for a player, never a token.** The access token lives in
//! `floptle-account` and is attached to requests there. A shipped game's Lua is
//! readable — anything a script can hold, somebody can read out of the file and
//! post somewhere — so it never holds one. That is also why `account.get` takes
//! a *path* rather than a URL: there is exactly one host it can reach.
//!
//! **Polled, not called back.** Signing in takes as long as a person takes to
//! pick up their phone, so `signIn` starts it and `state()` reports where it
//! got to. A sign-in screen is redrawing every frame anyway, and a callback for
//! something that can sit at "waiting" for a minute would be the awkward shape.
//!
//! **Play only, like `http.*`,** and for the same reason: a script being edited
//! must not reach a live endpoint because the Inspector re-ran it. Stop drops
//! every pending callback and abandons a sign-in in progress — but *not* the
//! session itself, which is stored in the OS keyring and shared with the Hub.
//! Signing in once covers every Play after it, and every other Floptle game.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use floptle_account::{Account, CloudReply, Phase};
use mlua::{Function, Lua, Value};

use crate::{LogLevel, ScriptLog};

/// How many Cloud calls may be in flight at once. Lower than `http.*`'s eight:
/// the Cloud API's own limit is 120 reads a minute, and a game that needs more
/// than a handful of simultaneous account calls is asking the wrong question.
const MAX_IN_FLIGHT: usize = 6;
/// Per-request timeout, seconds. Not configurable from Lua — one server, whose
/// timeouts we know.
const TIMEOUT: f64 = 20.0;

/// The `account.*` bridge.
pub(crate) struct AccountState {
    /// Built on FIRST USE, not at startup: constructing one reads the OS keyring
    /// (D-Bus on Linux), and a project that never signs anybody in should never
    /// pay for that or trip a "an app wants your keyring" prompt.
    account: Option<Account>,
    base: String,
    pending: HashMap<u64, Function>,
    tx: Sender<(u64, CloudReply)>,
    rx: Receiver<(u64, CloudReply)>,
    next_id: u64,
    playing: bool,
    warned_fixed: bool,
}

impl AccountState {
    pub(crate) fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        // A dev instance can be pointed at with an env var. Deliberately not a
        // project setting: a shipped game must not be able to carry a config
        // that sends its players' sign-ins somewhere else.
        let base = std::env::var("FLOPTLE_ACCOUNT_BASE")
            .unwrap_or_else(|_| floptle_account::DEFAULT_BASE.to_string());
        Self {
            account: None,
            base,
            pending: HashMap::new(),
            tx,
            rx,
            next_id: 0,
            playing: false,
            warned_fixed: false,
        }
    }

    /// The account, built the first time anything asks for one.
    fn account(&mut self) -> Account {
        self.account.get_or_insert_with(|| Account::new(self.base.clone())).clone()
    }

    /// The account **if one already exists** — for the read-only queries, which
    /// must not be the thing that triggers keyring I/O. `account.state()` in an
    /// `update()` would otherwise construct one on frame zero of every project.
    fn existing(&self) -> Option<&Account> {
        self.account.as_ref()
    }

    /// Stop / scene load: drop every waiting callback and abandon a sign-in in
    /// progress (its user code is stale the moment Play ends). The SESSION
    /// survives — it is the player's, not this run's.
    pub(crate) fn cancel_all(&mut self) {
        self.pending.clear();
        self.warned_fixed = false;
        if let Some(a) = &self.account {
            a.cancel_sign_in();
        }
    }

    pub(crate) fn set_playing(&mut self, playing: bool) {
        if self.playing && !playing {
            self.cancel_all();
        }
        self.playing = playing;
    }

    pub(crate) fn in_flight(&self) -> usize {
        self.pending.len()
    }

    /// Hand in a pre-built account so a test never constructs the real one — the
    /// real one reads the OS keyring, and a unit test that pops a
    /// "an app wants your passwords" prompt is a unit test nobody will run.
    #[cfg(test)]
    pub(crate) fn use_account(&mut self, a: Account) {
        self.account = Some(a);
    }
}

fn log(logs: &Rc<RefCell<Vec<ScriptLog>>>, level: LogLevel, msg: String) {
    logs.borrow_mut().push(ScriptLog { level, msg, source: None });
}

/// Deliver every Cloud reply that has arrived. Frame pass only — a reply's
/// arrival time is not reproducible and a replay must never see one.
pub(crate) fn drain(
    lua: &Lua,
    state: &Rc<RefCell<AccountState>>,
    logs: &Rc<RefCell<Vec<ScriptLog>>>,
) {
    // Collect with the borrow held, call with it RELEASED — a callback that
    // makes another request re-borrows the state.
    let ready: Vec<(CloudReply, Function)> = {
        let Ok(mut s) = state.try_borrow_mut() else { return };
        let mut out = Vec::new();
        while let Ok((id, reply)) = s.rx.try_recv() {
            if let Some(cb) = s.pending.remove(&id) {
                out.push((reply, cb));
            }
        }
        out
    };
    for (r, cb) in ready {
        // Always attempt the JSON parse: every Cloud endpoint answers JSON,
        // including its errors, so a script should never have to ask for it.
        match crate::http_api::make_reply_table(
            lua,
            r.status,
            &r.body,
            r.error.as_deref(),
            true,
        ) {
            Ok(t) => {
                if let Err(e) = cb.call::<()>(t) {
                    log(logs, LogLevel::Error, format!("account callback: {e}"));
                }
            }
            Err(e) => log(logs, LogLevel::Error, format!("account reply: {e}")),
        }
    }
}

/// `Phase` as the word a script matches on.
fn state_word(p: &Phase) -> &'static str {
    match p {
        Phase::SignedOut => "signedOut",
        Phase::Starting => "starting",
        Phase::Waiting { .. } => "waiting",
        Phase::SignedIn => "signedIn",
        Phase::Failed(_) => "failed",
    }
}

/// Start one Cloud request.
fn send(
    state: &Rc<RefCell<AccountState>>,
    logs: &Rc<RefCell<Vec<ScriptLog>>>,
    in_fixed: &Rc<std::cell::Cell<bool>>,
    method: &'static str,
    path: String,
    body: Option<String>,
    callback: Function,
) -> mlua::Result<()> {
    let mut s = state.borrow_mut();
    if !s.playing {
        return Err(mlua::Error::RuntimeError(
            "account is Play-only — edit mode never opens a socket".into(),
        ));
    }
    if in_fixed.get() && !s.warned_fixed {
        s.warned_fixed = true;
        drop(s);
        log(
            logs,
            LogLevel::Warn,
            "account called from fixedUpdate: a reply arrives when it arrives, so no replay can \
             reproduce it and a rollback match will diverge. Move it to update, start, a timer, \
             or an RPC handler."
                .into(),
        );
        s = state.borrow_mut();
    }
    if s.pending.len() >= MAX_IN_FLIGHT {
        return Err(mlua::Error::RuntimeError(format!(
            "account: {MAX_IN_FLIGHT} requests are already in flight — this is nearly always a \
             call inside update(); make it once and keep the answer"
        )));
    }
    let id = s.next_id;
    s.next_id += 1;
    let tx = s.tx.clone();
    let account = s.account();
    s.pending.insert(id, callback);
    drop(s);

    if let Err(e) =
        account.request(id, method, &path, body, Duration::from_secs_f64(TIMEOUT), tx)
    {
        // The worker never started, so nothing will ever answer this id — take
        // the callback back out rather than leaving it pending forever.
        state.borrow_mut().pending.remove(&id);
        return Err(mlua::Error::RuntimeError(format!("account: {e}")));
    }
    Ok(())
}

/// Sort `account.get(path, fn)` / `account.post(path, body, fn)` out of the args.
fn parse_args(
    args: Vec<Value>,
    has_body: bool,
) -> mlua::Result<(String, Option<String>, Function)> {
    let mut it = args.into_iter();
    let path = match it.next() {
        Some(Value::String(s)) => s.to_string_lossy().to_string(),
        _ => {
            return Err(mlua::Error::RuntimeError(
                "the first argument is a path like \"/wallet\" — not a full URL, because \
                 account.* only ever talks to fopull.com"
                    .into(),
            ));
        }
    };
    // Checked HERE, not on the worker, so a typo raises at the call site with a
    // line number instead of arriving three frames later as `res.error` — which
    // reads like the server rejected it rather than like the script is wrong.
    // `floptle-account` checks again on its own side; this is the friendly half.
    if let Err(why) = floptle_account::cloud::resolve(floptle_account::DEFAULT_BASE, &path) {
        return Err(mlua::Error::RuntimeError(format!(
            "{why} — account.* takes a path on fopull.com, like \"/wallet\" or \
             \"/games/fofighter/events\""
        )));
    }
    let body = if has_body {
        match it.next() {
            Some(Value::String(s)) => Some(s.to_string_lossy().to_string()),
            // Every Cloud body is a JSON OBJECT, and `{}` is what an empty Lua
            // table encodes to, so the common case needs no thought.
            Some(Value::Table(t)) => Some(
                serde_json::to_string(&crate::http_api::lua_to_json(&Value::Table(t))?)
                    .map_err(|e| mlua::Error::RuntimeError(format!("encoding the body: {e}")))?,
            ),
            Some(Value::Nil) | None => Some("{}".into()),
            Some(other) => {
                return Err(mlua::Error::RuntimeError(format!(
                    "the body is a table or a string, not a {}",
                    other.type_name()
                )));
            }
        }
    } else {
        None
    };
    match it.next() {
        Some(Value::Function(f)) => Ok((path, body, f)),
        _ => Err(mlua::Error::RuntimeError(
            "the last argument is the callback: function(res) ... end".into(),
        )),
    }
}

/// Install `account.*`.
pub(crate) fn install_account_api(
    lua: &Lua,
    state: Rc<RefCell<AccountState>>,
    logs: Rc<RefCell<Vec<ScriptLog>>>,
    in_fixed: Rc<std::cell::Cell<bool>>,
) {
    let Ok(t) = lua.create_table() else { return };

    // ---- the flow -----------------------------------------------------------
    let st = state.clone();
    if let Ok(f) = lua.create_function(move |_, ()| {
        let mut s = st.borrow_mut();
        if !s.playing {
            return Err(mlua::Error::RuntimeError(
                "account.signIn is Play-only — press Play and sign in there".into(),
            ));
        }
        s.account().sign_in();
        Ok(())
    }) {
        let _ = t.set("signIn", f);
    }

    let st = state.clone();
    if let Ok(f) = lua.create_function(move |_, ()| {
        if let Some(a) = st.borrow().existing() {
            a.cancel_sign_in();
        }
        Ok(())
    }) {
        let _ = t.set("cancel", f);
    }

    let st = state.clone();
    if let Ok(f) = lua.create_function(move |_, ()| {
        // Signing OUT builds an account if there isn't one, so that a game with
        // a Sign Out button still clears a session the Hub left behind.
        st.borrow_mut().account().sign_out();
        Ok(())
    }) {
        let _ = t.set("signOut", f);
    }

    // ---- what a screen draws ------------------------------------------------
    let st = state.clone();
    if let Ok(f) = lua.create_function(move |_, ()| {
        Ok(st.borrow().existing().map(|a| state_word(&a.phase())).unwrap_or("signedOut"))
    }) {
        let _ = t.set("state", f);
    }

    let st = state.clone();
    if let Ok(f) = lua.create_function(move |lua, ()| {
        let s = st.borrow();
        let Some(Phase::Waiting { user_code, url, expires_in }) = s.existing().map(|a| a.phase())
        else {
            return Ok(Value::Nil);
        };
        let t = lua.create_table()?;
        t.set("code", user_code)?;
        t.set("url", url)?;
        t.set("expiresIn", expires_in)?;
        Ok(Value::Table(t))
    }) {
        let _ = t.set("code", f);
    }

    let st = state.clone();
    if let Ok(f) = lua.create_function(move |lua, ()| {
        let s = st.borrow();
        let Some(session) = s.existing().and_then(|a| a.session()) else {
            return Ok(Value::Nil);
        };
        let t = lua.create_table()?;
        t.set("id", session.sub.as_str())?;
        t.set("name", session.player_name())?;
        if let Some(e) = session.email.as_deref() {
            t.set("email", e)?;
        }
        t.set("tier", session.tier.as_str())?;
        Ok(Value::Table(t))
    }) {
        let _ = t.set("player", f);
    }

    let st = state.clone();
    if let Ok(f) = lua.create_function(move |lua, ()| {
        let s = st.borrow();
        match s.existing().map(|a| a.phase()) {
            Some(Phase::Failed(e)) => Ok(Value::String(lua.create_string(&e)?)),
            _ => Ok(Value::Nil),
        }
    }) {
        let _ = t.set("error", f);
    }

    let st = state.clone();
    if let Ok(f) = lua.create_function(move |_, ()| Ok(st.borrow().in_flight())) {
        let _ = t.set("inFlight", f);
    }

    // ---- the Cloud calls ----------------------------------------------------
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
            let (path, body, cb) = parse_args(args.into_iter().collect(), has_body)?;
            send(&st, &lg, &fx, method, path, body, cb)
        }) {
            let _ = t.set(name, f);
        }
    }

    let _ = lua.globals().set("account", t);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host with the account API installed, without a driver.
    fn harness() -> (Lua, Rc<RefCell<AccountState>>) {
        let lua = Lua::new();
        let state = Rc::new(RefCell::new(AccountState::new()));
        let logs = Rc::new(RefCell::new(Vec::new()));
        install_account_api(&lua, state.clone(), logs, Rc::new(std::cell::Cell::new(false)));
        (lua, state)
    }

    #[test]
    fn a_project_that_never_signs_in_never_touches_the_keyring() {
        let (lua, state) = harness();
        // The read-only queries answer for a signed-out player WITHOUT
        // constructing an Account — which is what would read the OS keyring and
        // pop a permission prompt on someone who only wanted to play the game.
        let s: String = lua.load("return account.state()").eval().unwrap();
        assert_eq!(s, "signedOut");
        assert_eq!(lua.load("return account.player()").eval::<Value>().unwrap(), Value::Nil);
        assert_eq!(lua.load("return account.code()").eval::<Value>().unwrap(), Value::Nil);
        assert_eq!(lua.load("return account.error()").eval::<Value>().unwrap(), Value::Nil);
        assert!(state.borrow().existing().is_none(), "no Account should have been built");
    }

    #[test]
    fn the_whole_surface_is_play_only() {
        let (lua, _state) = harness();
        // playing is false by default — nothing that opens a socket may run.
        let e = lua.load("account.signIn()").exec().unwrap_err().to_string();
        assert!(e.contains("Play-only"), "got {e}");
        let e = lua.load("account.get('/wallet', function() end)").exec().unwrap_err().to_string();
        assert!(e.contains("Play-only"), "got {e}");
    }

    #[test]
    fn a_url_where_a_path_belongs_says_so() {
        let (lua, state) = harness();
        state.borrow_mut().set_playing(true);
        // The mistake everybody makes coming from http.*, and the error has to
        // explain WHY rather than just refusing.
        let e = lua
            .load("account.get('https://fopull.com/api/floptle/v1/wallet', function() end)")
            .exec()
            .unwrap_err()
            .to_string();
        assert!(e.contains("path"), "got {e}");
        assert!(e.contains("fopull.com"), "the reason should be in the message, got {e}");
    }

    #[test]
    fn a_missing_callback_is_refused_rather_than_dropped() {
        let (lua, state) = harness();
        state.borrow_mut().set_playing(true);
        for call in ["account.get('/wallet')", "account.post('/x', {})"] {
            let e = lua.load(call).exec().unwrap_err().to_string();
            assert!(e.contains("callback"), "{call} gave {e}");
        }
        assert_eq!(state.borrow().in_flight(), 0, "nothing should be left pending");
    }

    #[test]
    fn a_request_with_nobody_signed_in_answers_the_callback() {
        let (lua, state) = harness();
        {
            let mut s = state.borrow_mut();
            s.set_playing(true);
            // A signed-out account with a provider that would panic if touched:
            // "nobody is signed in" must be answered locally, not asked about.
            s.use_account(Account::with(
                "https://fopull.com",
                std::sync::Arc::new(floptle_account::MemoryStore::default()),
                std::sync::Arc::new(|_| panic!("a signed-out request must not reach the network")),
            ));
        }
        lua.load(
            "got = nil
             account.post('/games/x/events', { event = 'e' }, function(res) got = res end)",
        )
        .exec()
        .unwrap();
        assert_eq!(state.borrow().in_flight(), 1);
        // The reply lands on a later frame, exactly like http.*.
        let logs = Rc::new(RefCell::new(Vec::new()));
        for _ in 0..200 {
            drain(&lua, &state, &logs);
            if state.borrow().in_flight() == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let ok: Value = lua.load("return got and got.ok").eval().unwrap();
        assert_eq!(ok, Value::Boolean(false));
        let err: String = lua.load("return got.error").eval().unwrap();
        assert!(err.contains("signed in"), "got {err}");
    }

    #[test]
    fn stopping_play_drops_the_waiting_callbacks() {
        let (lua, state) = harness();
        {
            let mut s = state.borrow_mut();
            s.set_playing(true);
            s.use_account(Account::with(
                "https://fopull.com",
                std::sync::Arc::new(floptle_account::MemoryStore::default()),
                std::sync::Arc::new(|_| panic!("a signed-out request must not reach the network")),
            ));
        }
        lua.load("account.post('/games/x/events', { event = 'e' }, function() end)")
            .exec()
            .unwrap();
        assert_eq!(state.borrow().in_flight(), 1);
        state.borrow_mut().set_playing(false);
        assert_eq!(state.borrow().in_flight(), 0, "Stop must not leave callbacks armed");
    }
}
