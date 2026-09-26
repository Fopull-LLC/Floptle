//! `account.*` — the player's Foverse account, from Lua.
//!
//! ```lua
//! account.signIn()                       -- begins; returns immediately
//! account.state()                        -- "signedOut" | "starting" | "waiting" | "signedIn" | "failed"
//! account.code()                         -- while waiting: { code = "wxyz-9999", url = "…" }
//! account.player()                       -- when signed in: { id, name, email, tier }
//! account.get("/wallet", function(res) end)
//! account.post("/games/fofighter/events", { event = "cpu_match_won" }, function(res) end)
//! ```
//!
//! A script asks for a player, never a token. The access token lives in
//! `floptle-account` and is attached to requests there; a shipped game's Lua is
//! readable, so it never holds one. `account.get` takes a path rather than a
//! URL for the same reason: there is exactly one host it can reach.
//!
//! Polled, not called back. Signing in takes as long as a person takes to pick
//! up their phone, so `signIn` starts it and `state()` reports where it got
//! to; a sign-in screen is redrawing every frame anyway.
//!
//! Play only, like `http.*`: a script being edited cannot reach a live endpoint
//! because the Inspector re-ran it. Stop drops every pending callback and
//! abandons a sign-in in progress, but not the session itself, which is stored
//! in the OS keyring and shared with the Hub. Signing in once covers every
//! Play after it, and every other Floptle game.

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
    /// Built on first USE, not at startup: constructing one reads the OS keyring
    /// (D-Bus on Linux), and a project that never signs anybody in should never
    /// pay for that or trip a "an app wants your keyring" prompt.
    account: Option<Account>,
    /// How to build it: the real one reads the OS keyring, and a test hands in
    /// one that reads memory instead.
    make: Rc<dyn Fn(&str) -> Account>,
    base: String,
    /// The callback, and whether it asked for a blob (bytes, not JSON).
    pending: HashMap<u64, (Function, bool)>,
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
            make: Rc::new(|base: &str| Account::new(base)),
            base,
            pending: HashMap::new(),
            tx,
            rx,
            next_id: 0,
            playing: false,
            warned_fixed: false,
        }
    }

    /// The account, built the first time anything asks for one. Building it
    /// starts the off-thread read of the stored session.
    fn account(&mut self) -> Account {
        let make = self.make.clone();
        self.account.get_or_insert_with(|| make(&self.base)).clone()
    }

    /// The account **if one already exists**, for the calls that have no
    /// reason to read the keyring: cancelling a sign-in that never started.
    fn existing(&self) -> Option<&Account> {
        self.account.as_ref()
    }

    /// The account a "who is signed in?" question reads.
    ///
    /// During Play the question is the reason to look, so it builds one, and
    /// the stored session (the Hub's, or this game's last sign-in) is read
    /// without a Cloud call. Otherwise every scene a game opened answered
    /// "signed out" until something happened to call the Cloud. Outside Play
    /// it only reads one that exists: a script being edited must not be what
    /// reads the keyring.
    fn asked(&mut self) -> Option<Account> {
        if self.playing { Some(self.account()) } else { self.account.clone() }
    }

    /// Stop / scene load: drop every waiting callback and abandon a sign-in in
    /// progress (its user code is stale the moment Play ends). The session
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

    /// Hand in how to build the account, for a test of what building it does.
    #[cfg(test)]
    pub(crate) fn use_maker(&mut self, make: impl Fn(&str) -> Account + 'static) {
        self.make = Rc::new(make);
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
    // Collect with the borrow held, call with it released — a callback that
    // makes another request re-borrows the state.
    let ready: Vec<(CloudReply, (Function, bool))> = {
        let Ok(mut s) = state.try_borrow_mut() else { return };
        let mut out = Vec::new();
        while let Ok((id, reply)) = s.rx.try_recv() {
            if let Some(cb) = s.pending.remove(&id) {
                out.push((reply, cb));
            }
        }
        out
    };
    for (r, (cb, blob)) in ready {
        // Every Cloud endpoint answers JSON, errors included, so a script never
        // has to ask for the parse. A blob answers its bytes, which are parsed
        // only when the server says they are JSON (a refusal).
        match crate::http_api::make_reply_table(
            lua,
            r.status,
            &r.body,
            r.error.as_deref(),
            !blob || r.said_json,
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
    body: Option<Vec<u8>>,
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
    s.pending.insert(id, (callback, floptle_account::cloud::is_blob_path(&path)));
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
) -> mlua::Result<(String, Option<Vec<u8>>, Function)> {
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
    // Checked here, not on the worker, so a typo raises at the call site with a
    // line number instead of arriving three frames later as `res.error` — which
    // reads like the server rejected it rather than like the script is wrong.
    // `floptle-account` checks again on its own side; this is the friendly half.
    if let Err(why) = floptle_account::cloud::resolve(floptle_account::DEFAULT_BASE, &path) {
        return Err(mlua::Error::RuntimeError(format!(
            "{why} — account.* takes a path on fopull.com, like \"/wallet\" or \
             \"/games/fofighter/events\""
        )));
    }
    // A blob is bytes: the string goes out exactly as it is. Every other body
    // is JSON.
    let blob = floptle_account::cloud::is_blob_path(&path);
    let body = if has_body {
        match it.next() {
            Some(Value::String(s)) => Some(s.as_bytes().to_vec()),
            Some(Value::Table(_)) if blob => {
                return Err(mlua::Error::RuntimeError(format!(
                    "{path} is a blob, which is bytes: pass a string (a file's contents, a \
                     picture's res.body), not a table"
                )));
            }
            // Every Cloud body is a JSON object, and `{}` is what an empty Lua
            // table encodes to, so the common case needs no thought.
            Some(Value::Table(t)) => Some(
                serde_json::to_vec(&crate::http_api::lua_to_json(&Value::Table(t))?)
                    .map_err(|e| mlua::Error::RuntimeError(format!("encoding the body: {e}")))?,
            ),
            Some(Value::Nil) | None if blob => Some(Vec::new()),
            Some(Value::Nil) | None => Some(b"{}".to_vec()),
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
        // Signing out builds an account if there isn't one, so that a game with
        // a Sign Out button still clears a session the Hub left behind.
        st.borrow_mut().account().sign_out();
        Ok(())
    }) {
        let _ = t.set("signOut", f);
    }

    // ---- what a screen draws ------------------------------------------------
    let st = state.clone();
    if let Ok(f) = lua.create_function(move |_, ()| {
        let Some(a) = st.borrow_mut().asked() else { return Ok("signedOut") };
        let phase = a.phase();
        // Still reading the stored session: not signed out yet, just not known.
        if a.is_restoring() && matches!(phase, Phase::SignedOut) {
            return Ok("starting");
        }
        Ok(state_word(&phase))
    }) {
        let _ = t.set("state", f);
    }

    let st = state.clone();
    if let Ok(f) = lua.create_function(move |lua, ()| {
        let asked = st.borrow_mut().asked();
        let Some(Phase::Waiting { user_code, url, expires_in }) = asked.map(|a| a.phase())
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
        let asked = st.borrow_mut().asked();
        let Some(session) = asked.and_then(|a| a.session()) else {
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
        let asked = st.borrow_mut().asked();
        match asked.map(|a| a.phase()) {
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
        // Outside Play the read-only queries answer for a signed-out player
        // without constructing an Account — which is what would read the OS
        // keyring and pop a permission prompt on someone editing a script.
        let s: String = lua.load("return account.state()").eval().unwrap();
        assert_eq!(s, "signedOut");
        assert_eq!(lua.load("return account.player()").eval::<Value>().unwrap(), Value::Nil);
        assert_eq!(lua.load("return account.code()").eval::<Value>().unwrap(), Value::Nil);
        assert_eq!(lua.load("return account.error()").eval::<Value>().unwrap(), Value::Nil);
        assert!(state.borrow().existing().is_none(), "no Account should have been built");
    }

    /// A keyring that answers when the test says so, so "still reading" is a
    /// state the test can stand in rather than race.
    struct GatedStore {
        open: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        session: floptle_account::Session,
    }

    impl floptle_account::TokenStore for GatedStore {
        fn save(&self, _: &floptle_account::Session) -> Result<(), String> {
            Ok(())
        }
        fn load(&self) -> Option<floptle_account::Session> {
            self.open.lock().ok()?.recv().ok()?;
            Some(self.session.clone())
        }
        fn clear(&self) -> Result<(), String> {
            Ok(())
        }
    }

    /// A game that asks who is signed in gets the stored session (the Hub's,
    /// or its own last sign-in) with no Cloud call, in every scene: a scene
    /// load drops callbacks and an unfinished sign-in, never the player. The
    /// provider panics if touched, so this proves no request went out.
    #[test]
    fn a_stored_session_is_signed_in_in_every_scene_without_a_request() {
        let (lua, state) = harness();
        let (release, gate) = std::sync::mpsc::channel();
        let store = std::sync::Arc::new(GatedStore {
            open: std::sync::Mutex::new(gate),
            session: floptle_account::Session {
                sub: "p-1".into(),
                name: Some("Ada".into()),
                email: None,
                tier: "free".into(),
                access_token: "opaque".into(),
                refresh_token: None,
            },
        });
        {
            let mut s = state.borrow_mut();
            s.set_playing(true);
            s.use_maker(move |base| {
                let a = Account::with(
                    base,
                    store.clone(),
                    std::sync::Arc::new(|_| panic!("reading the stored session must not call the Cloud")),
                );
                a.restore();
                a
            });
        }
        let now = |lua: &Lua| -> String { lua.load("return account.state()").eval().unwrap() };
        // Asking is what starts the read; while it runs the answer is not
        // "signed out", or a sign-in screen flashes at a signed-in player.
        assert_eq!(now(&lua), "starting");
        release.send(()).unwrap();
        let mut got = now(&lua);
        for _ in 0..400 {
            if got != "starting" {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
            got = now(&lua);
        }
        assert_eq!(got, "signedIn");
        let name: String = lua.load("return account.player().name").eval().unwrap();
        assert_eq!(name, "Ada");
        // `scene.load`.
        state.borrow_mut().cancel_all();
        assert_eq!(now(&lua), "signedIn", "a scene load must not sign the player out");
        assert_eq!(state.borrow().in_flight(), 0, "and no request was made to find that out");
    }

    /// A blob is bytes: a table sent to one is refused at the call, and a
    /// blob's reply reaches the script whole rather than failing the JSON
    /// parse every other endpoint's reply goes through.
    #[test]
    fn a_blob_is_sent_as_bytes_and_its_reply_is_not_parsed_as_json() {
        let (lua, state) = harness();
        state.borrow_mut().set_playing(true);
        let e = lua
            .load("account.put('/games/g/blobs/pics/a', { x = 1 }, function() end)")
            .exec()
            .unwrap_err()
            .to_string();
        assert!(e.contains("is a blob, which is bytes"), "got {e}");

        let logs = Rc::new(RefCell::new(Vec::new()));
        for (id, blob) in [(1u64, true), (2, false)] {
            let cb = lua
                .create_function(move |lua, res: mlua::Table| {
                    let body: mlua::String = res.get("body")?;
                    lua.globals().set(format!("ok{id}"), res.get::<bool>("ok")?)?;
                    lua.globals().set(format!("len{id}"), body.as_bytes().len())?;
                    Ok(())
                })
                .unwrap();
            let mut s = state.borrow_mut();
            s.pending.insert(id, (cb, blob));
            let body: Vec<u8> = (0..=255u8).collect();
            s.tx.send((id, CloudReply { status: 200, body, error: None, said_json: false })).unwrap();
        }
        drain(&lua, &state, &logs);
        let g = lua.globals();
        assert!(g.get::<bool>("ok1").unwrap(), "a blob's bytes were parsed as JSON");
        assert_eq!(g.get::<usize>("len1").unwrap(), 256);
        assert!(!g.get::<bool>("ok2").unwrap(), "a JSON endpoint's bytes were not parsed");
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
        // explain why rather than just refusing.
        let e = lua
            .load("account.get('https://fopull.com/api/floptle/v1/wallet', function() end)")
            .exec()
            .unwrap_err()
            .to_string();
        assert!(e.contains("path"), "got {e}");
        assert!(e.contains("fopull.com"), "the reason should be in the message, got {e}");
    }

    /// **A script acts as the player, never as the developer.** The token
    /// behind `account.*` is the Hub's, and it can rotate keys; a game's Lua
    /// asking for `/cloud/...` is refused at the call with the rule named and
    /// nothing is left pending. The player's surface still works.
    #[test]
    fn a_script_is_refused_the_developer_endpoints_at_the_call() {
        let (lua, state) = harness();
        state.borrow_mut().set_playing(true);
        for call in [
            "account.get('/cloud/games', function() end)",
            "account.post('/cloud/games/fofighter/key', {}, function() end)",
            "account.get('/userinfo', function() end)",
        ] {
            let e = lua.load(call).exec().unwrap_err().to_string();
            assert!(e.contains("not a path a game may call"), "{call} gave {e}");
        }
        assert_eq!(state.borrow().in_flight(), 0, "a refused call left something pending");
        // `/wallet` gets past the path check (and stops at the sign-in gate,
        // which is the next thing a request meets — not the path rule).
        let e = lua.load("account.get('/wallet', function() end)").exec().err().map(|e| e.to_string());
        assert!(
            e.as_deref().is_none_or(|e| !e.contains("not a path a game may call")),
            "/wallet was refused by the path rule: {e:?}"
        );
    }

    #[test]
    fn a_missing_callback_is_refused_rather_than_dropped() {
        let (lua, state) = harness();
        state.borrow_mut().set_playing(true);
        for call in ["account.get('/wallet')", "account.post('/games/x/events', {})"] {
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

/// Against the real fopull.com, as the player signed in to the Hub on this
/// machine (the keyring session). `#[ignore]`d: they need the network and a
/// signed-in account. `cargo test -p floptle-script -- --ignored live_ --nocapture`.
#[cfg(test)]
mod live_tests {
    use super::*;
    use mlua::Table;

    /// Card 0054's last two criteria, from Lua: register a game slug, write
    /// and read back a cloud save, submit a score and read the board, then
    /// delete the game so nothing is left behind.
    #[test]
    #[ignore = "hits fopull.com as the signed-in player"]
    fn live_a_game_saves_and_scores_from_lua_as_the_signed_in_player() {
        let lua = Lua::new();
        let state = Rc::new(RefCell::new(AccountState::new()));
        let logs: Rc<RefCell<Vec<ScriptLog>>> = Rc::new(RefCell::new(Vec::new()));
        install_account_api(&lua, state.clone(), logs.clone(), Rc::new(std::cell::Cell::new(false)));
        state.borrow_mut().set_playing(true);
        lua.load(
            r#"
            seen = {}
            local slug = "engine-smoke-test"
            local function note(name, r) seen[#seen + 1] = { name = name, status = r.status, body = r.body } end
            function go()
              account.post("/games", { slug = slug, name = "Engine Smoke Test" }, function(r)
                note("register", r)
                account.put("/games/" .. slug .. "/saves/autosave",
                  { data = { level = 3, hp = 57, inventory = json.array({ "rope", "lamp" }) } }, function(r)
                  note("save put", r)
                  account.get("/games/" .. slug .. "/saves/autosave", function(r)
                    note("save get", r)
                    saved = r.json and r.json.data
                    account.post("/games/" .. slug .. "/scores", { score = 1200, meta = { run = "first" } }, function(r)
                      note("score", r)
                      account.get("/games/" .. slug .. "/leaderboard", function(r)
                        note("board", r)
                        best = r.json and r.json.you and r.json.you.score
                        account.delete("/games/" .. slug, function(r) note("cleanup", r); done = true end)
                      end)
                    end)
                  end)
                end)
              end)
            end
            "#,
        )
        .exec()
        .unwrap();

        // The keyring read is on a worker; wait for it to say who is here.
        let state_of = || lua.load("return account.state()").eval::<String>().unwrap();
        let t = std::time::Instant::now();
        while state_of() != "signedIn" && t.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // Nobody stored (or no keyring reachable): the device flow, which a
        // person approves in a browser. The code is printed; up to 5 minutes.
        if state_of() != "signedIn" {
            lua.load("account.signIn()").exec().unwrap();
            let mut shown = false;
            let t = std::time::Instant::now();
            while state_of() != "signedIn" && t.elapsed() < std::time::Duration::from_secs(300) {
                if !shown && let Ok(c) = lua.load("local c = account.code(); return c and (c.url .. '  code ' .. c.code)").eval::<String>() {
                    println!("APPROVE: {c}");
                    shown = true;
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }
        assert_eq!(state_of(), "signedIn", "nobody signed in: {:?}", lua.load("return account.error()").eval::<Option<String>>());
        let who: String = lua.load("return account.player().name").eval().unwrap_or_default();
        println!("signed in as {who}");

        lua.load("go()").exec().unwrap();
        let t = std::time::Instant::now();
        while !lua.globals().get::<bool>("done").unwrap_or(false) && t.elapsed() < std::time::Duration::from_secs(60) {
            drain(&lua, &state, &logs);
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let seen: Table = lua.globals().get("seen").unwrap();
        for row in seen.sequence_values::<Table>() {
            let row = row.unwrap();
            let body: String = row.get("body").unwrap_or_default();
            println!("{:>9} -> {} {}", row.get::<String>("name").unwrap(), row.get::<u16>("status").unwrap_or(0), &body[..body.len().min(240)]);
        }
        assert!(lua.globals().get::<bool>("done").unwrap_or(false), "the chain did not finish");
        let saved: Table = lua.globals().get("saved").expect("the save did not read back");
        assert_eq!(saved.get::<i64>("level").unwrap(), 3);
        assert_eq!(saved.get::<i64>("hp").unwrap(), 57);
        let inv: Table = saved.get("inventory").unwrap();
        assert_eq!(inv.get::<String>(2).unwrap(), "lamp");
        assert_eq!(lua.globals().get::<i64>("best").unwrap(), 1200, "the board did not show the score");
    }
}
