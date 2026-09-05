//! [`Account`] — the device flow, made non-blocking.
//!
//! Signing in takes as long as a person takes to pick up their phone, open a
//! browser and type a code. A game cannot wait for that, so nothing here
//! blocks: every network step runs on a worker thread and the caller reads a
//! [`Phase`] whenever it likes — once a frame, from a HUD, forever if the player
//! wanders off.
//!
//! The same rule covers the boring paths. Reading the stored session is keyring
//! I/O (D-Bus on Linux) and *that* can hang on a machine with no secret service,
//! so even the restore-on-startup runs off-thread. A game that never signs in
//! should never notice this module exists.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::auth::{self, Provider, RefreshError, Session, TokenStore};
// Only the device flow's `Provider` reads entitlements synchronously; the
// browser fetches them in `web_auth::browser::identify`.
#[cfg(not(target_arch = "wasm32"))]
use crate::auth::Entitlements;
// The desktop's secret store and the desktop's transport. A browser build gets
// `auth::WebStore` and `auth::OfflineProvider` instead — see `Account::new`.
#[cfg(not(target_arch = "wasm32"))]
use crate::auth::KeyringStore;
#[cfg(not(target_arch = "wasm32"))]
use crate::cloud;
use crate::cloud::CloudReply;

/// Where a sign-in has got to. A caller draws this and nothing else.
#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    /// Nobody is signed in and nothing is happening.
    SignedOut,
    /// Asking the provider for a code. Brief, but it is a network round trip and
    /// a game that shows nothing here looks frozen on a slow connection.
    Starting,
    /// Show the player `user_code` and send them to `url`.
    Waiting { user_code: String, url: String, expires_in: u64 },
    /// Done — [`Account::session`] has the player.
    SignedIn,
    /// It failed, and this is what to tell the player.
    Failed(String),
}

impl Phase {
    /// Whether a sign-in is in progress, so a second button press doesn't start
    /// a second flow with a second code.
    pub fn is_busy(&self) -> bool {
        matches!(self, Phase::Starting | Phase::Waiting { .. })
    }
}

struct Inner {
    phase: Phase,
    session: Option<Session>,
}

/// How a worker builds its provider. Injected so the whole flow is testable
/// offline against `auth`'s existing mock, exactly as the Hub tests it.
type MakeProvider = Arc<dyn Fn(&str) -> Box<dyn Provider + Send> + Send + Sync>;

/// The player's account: sign-in state, the stored session, and authorized
/// calls to Floptle Cloud.
///
/// Cheap to clone (three `Arc`s) — the Lua bridge, the editor and a worker
/// thread all hold the same one.
#[derive(Clone)]
pub struct Account {
    base: String,
    inner: Arc<Mutex<Inner>>,
    cancel: Arc<AtomicBool>,
    store: Arc<dyn TokenStore + Send + Sync>,
    make_provider: MakeProvider,
    /// Held across a token refresh so two concurrent requests don't both spend
    /// the refresh token — the second one waits, then finds the session already
    /// fresh and uses it.
    refreshing: Arc<Mutex<()>>,
}

/// Run `f` off the caller's thread — or, in a browser, on it.
///
/// `wasm32-unknown-unknown` has no threads: `std::thread::spawn` **compiles**
/// and then fails at runtime, which is the worst of the two possible answers.
/// Everything routed through here is either a store read or a call whose very
/// first statement the browser's `Provider` refuses (see
/// [`auth::OfflineProvider`]), so running it inline there costs the page
/// nothing and cannot spin.
fn detach(name: &str, f: impl FnOnce() + Send + 'static) -> Result<(), String> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::thread::Builder::new()
            .name(name.into())
            .spawn(f)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = name;
        f();
        Ok(())
    }
}

impl Account {
    /// The real thing: the OS keyring and an HTTP provider pointed at `base`.
    /// Restores a stored session in the background — a session shared with the
    /// Hub, so a player already signed in there is signed in here.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new(base: impl Into<String>) -> Self {
        let me = Self::with(
            base,
            Arc::new(KeyringStore::default()),
            Arc::new(|base: &str| Box::new(auth::HttpProvider::new(base)) as Box<dyn Provider + Send>),
        );
        me.restore();
        me
    }

    /// The browser's pair: `localStorage` for the session, and a provider that
    /// refuses every network call with the reason (see
    /// [`auth::OfflineProvider`]).
    ///
    /// The store is real and the restore still runs, deliberately: the shape a
    /// signed-in session takes is settled here, so when fopull.com's side of
    /// the contract lands there is nothing left to design on this side.
    #[cfg(target_arch = "wasm32")]
    pub fn new(base: impl Into<String>) -> Self {
        let me = Self::with(
            base,
            Arc::new(auth::WebStore::default()),
            Arc::new(|_base: &str| Box::new(auth::OfflineProvider) as Box<dyn Provider + Send>),
        );
        me.restore();
        // Order matters: `restore` reads the stored session synchronously, and
        // `web_restore` then either finishes a sign-in this load is returning
        // from or refreshes what `restore` just found. The other way round, a
        // completed sign-in would be overwritten by the stale stored session.
        me.web_restore();
        me
    }

    /// Injectable constructor for tests. Does **not** restore — a test says when.
    pub fn with(
        base: impl Into<String>,
        store: Arc<dyn TokenStore + Send + Sync>,
        make_provider: MakeProvider,
    ) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            inner: Arc::new(Mutex::new(Inner { phase: Phase::SignedOut, session: None })),
            cancel: Arc::new(AtomicBool::new(false)),
            store,
            make_provider,
            refreshing: Arc::new(Mutex::new(())),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn phase(&self) -> Phase {
        self.inner.lock().map(|i| i.phase.clone()).unwrap_or(Phase::SignedOut)
    }

    pub fn session(&self) -> Option<Session> {
        self.inner.lock().ok().and_then(|i| i.session.clone())
    }

    pub fn is_signed_in(&self) -> bool {
        self.session().is_some()
    }

    fn set_phase(&self, p: Phase) {
        if let Ok(mut i) = self.inner.lock() {
            i.phase = p;
        }
    }

    /// Load a stored session off-thread. Idempotent and silent: no stored
    /// session is the ordinary case, not a failure worth reporting.
    pub fn restore(&self) {
        let me = self.clone();
        let _ = detach("floptle-account-restore", move || {
            let Some(session) = me.store.load() else { return };
            // Minted by a provider we no longer point at — see `Session::issued_by`. It can
            // only 401, so it is forgotten rather than shown as a signed-in player whose
            // every call fails. The Hub shares this entry and applies the same rule.
            if !session.issued_by(&me.base) {
                let _ = me.store.clear();
                return;
            }
            if let Ok(mut i) = me.inner.lock() {
                // A sign-in that started in the meantime wins — it is the more
                // recent statement of what the player wants.
                if i.session.is_none() && !i.phase.is_busy() {
                    i.session = Some(session);
                    i.phase = Phase::SignedIn;
                }
            }
        });
    }

    /// Begin the device flow. Returns immediately; watch [`Account::phase`].
    /// A second call while one is running is ignored rather than issuing a
    /// second code — two live codes is the fastest way to make a player type
    /// the wrong one.
    pub fn sign_in(&self) {
        {
            let Ok(mut i) = self.inner.lock() else { return };
            if i.phase.is_busy() {
                return;
            }
            i.phase = Phase::Starting;
        }
        // **In a browser the sign-in is a navigation, not a poll.** There is no
        // user code to show and nothing to wait for here: the page leaves, and
        // the flow finishes in `web_restore` when it comes back. See
        // `web_auth` and contract §6.
        #[cfg(target_arch = "wasm32")]
        {
            let client = self.web_client();
            if let Err(e) = crate::web_auth::browser::start(&client) {
                self.set_phase(Phase::Failed(e));
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
        self.cancel.store(false, Ordering::Relaxed);
        let me = self.clone();
        if let Err(e) = detach("floptle-account-signin", move || me.run_sign_in()) {
            self.set_phase(Phase::Failed(format!("could not start the sign-in: {e}")));
        }
        }
    }

    /// The page this build redirects back to, and the URI that has to be
    /// registered against the game (contract §6.4).
    ///
    /// Defaults to the game's own address with the query stripped, so a build
    /// works with one registration and no configuration. That means signing in
    /// reloads the game — acceptable at a menu, which is where it belongs, and
    /// it is the shape that survives a sandboxed iframe where a popup would be
    /// blocked outright.
    #[cfg(target_arch = "wasm32")]
    fn web_client(&self) -> crate::web_auth::WebClient {
        let redirect = crate::web_auth::browser::current_page_url().unwrap_or_default();
        crate::web_auth::WebClient::new(self.base.clone(), redirect)
    }

    /// Finish a sign-in the page is returning from, and refresh a stored session
    /// that has aged out. Called once at boot, after `restore`.
    ///
    /// Both halves are async because `fetch` is; neither blocks the frame. An
    /// ordinary page load — the overwhelming majority — does nothing at all here
    /// and reports nothing, because every boot runs it.
    #[cfg(target_arch = "wasm32")]
    pub fn web_restore(&self) {
        let me = self.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let client = me.web_client();
            match crate::web_auth::browser::complete(&client).await {
                Ok(Some(tokens)) => match crate::web_auth::browser::identify(&client, tokens).await {
                    Ok(session) => me.adopt(session),
                    Err(e) => me.set_phase(Phase::Failed(e)),
                },
                Ok(None) => me.web_refresh_if_stale(&client).await,
                Err(e) => me.set_phase(Phase::Failed(e)),
            }
        });
    }

    /// A stored session whose access token has expired is refreshed once, at
    /// boot. The browser's access token is 900s (§6.5), so this is the ordinary
    /// path on any return visit, not an edge case.
    #[cfg(target_arch = "wasm32")]
    async fn web_refresh_if_stale(&self, client: &crate::web_auth::WebClient) {
        let now = unix_now();
        let Some(session) = self.session().filter(|s| s.needs_refresh(now)) else { return };
        match crate::web_auth::browser::refresh(client, &session).await {
            // Persist BEFORE anything else can call: the old refresh token died
            // the moment this succeeded, so a crash between here and the save
            // costs the session.
            Ok(fresh) => self.adopt(fresh),
            // A refresh that fails is a sign-out, not a retry — the token is
            // either rotated away or revoked, and both are terminal.
            Err(e) => {
                let _ = self.store.clear();
                if let Ok(mut i) = self.inner.lock() {
                    i.session = None;
                    i.phase = Phase::Failed(e);
                }
            }
        }
    }

    /// Take a session as the current one and persist it.
    #[cfg(target_arch = "wasm32")]
    fn adopt(&self, session: crate::auth::Session) {
        if let Err(e) = self.store.save(&session) {
            log::warn!("could not store the session: {e}");
        }
        if let Ok(mut i) = self.inner.lock() {
            i.session = Some(session);
            i.phase = Phase::SignedIn;
        }
    }

    // The device flow's poll loop. A browser never runs it: §6.1 refuses
    // `floptle-web` a device grant and §6.3 leaves `/oauth/device` off CORS on
    // purpose, because a page can redirect.
    #[cfg(not(target_arch = "wasm32"))]
    fn run_sign_in(&self) {
        let provider = (self.make_provider)(&self.base);
        let pkce = auth::Pkce::generate();
        let dc = match provider.start_device(&pkce.challenge) {
            Ok(d) => d,
            Err(e) => return self.set_phase(Phase::Failed(e)),
        };
        self.set_phase(Phase::Waiting {
            user_code: dc.user_code.clone(),
            url: dc.approve_url().to_string(),
            expires_in: dc.expires_in,
        });
        // Sleep in slices so Cancel is felt in a quarter of a second rather than
        // at the end of a five-second poll interval.
        let cancel = self.cancel.clone();
        let sleep = |secs: u64| {
            let deadline = std::time::Instant::now() + Duration::from_secs(secs);
            while std::time::Instant::now() < deadline {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        };
        let tokens = match auth::poll_until(
            &*provider,
            &dc.device_code,
            &pkce.verifier,
            dc.interval,
            dc.expires_in,
            &self.cancel,
            sleep,
        ) {
            Ok(t) => t,
            Err(e) => return self.set_phase(Phase::Failed(e)),
        };
        let who = match provider.userinfo(&tokens.access_token) {
            Ok(w) => w,
            Err(e) => return self.set_phase(Phase::Failed(e)),
        };
        // The plan is decoration — a sign-in that worked must not be reported as
        // a failure because the entitlements endpoint had a bad minute.
        let ent = provider.entitlements(&tokens.access_token).unwrap_or_else(|e| {
            log::warn!("could not read the account plan: {e}");
            Entitlements::default()
        });
        let session = Session::from_parts(tokens, who, ent);
        if let Err(e) = self.store.save(&session) {
            // Not fatal: the player is signed in for this run, they just have to
            // do it again next time.
            log::warn!("could not store the session: {e}");
        }
        if let Ok(mut i) = self.inner.lock() {
            i.session = Some(session);
            i.phase = Phase::SignedIn;
        }
    }

    /// Abandon a sign-in in progress. Harmless at any other time.
    pub fn cancel_sign_in(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        if let Ok(mut i) = self.inner.lock()
            && i.phase.is_busy()
        {
            i.phase = if i.session.is_some() { Phase::SignedIn } else { Phase::SignedOut };
        }
    }

    /// Sign out: forget the session **now**, then clear the store and revoke the
    /// refresh token in the background. In that order deliberately — a player
    /// who presses Sign Out is signed out whether or not the network agrees.
    pub fn sign_out(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        let old = {
            let Ok(mut i) = self.inner.lock() else { return };
            i.phase = Phase::SignedOut;
            i.session.take()
        };
        let me = self.clone();
        let _ = detach("floptle-account-signout", move || {
            if let Err(e) = me.store.clear() {
                log::warn!("could not clear the stored session: {e}");
            }
            if let Some(rt) = old.and_then(|s| s.refresh_token) {
                let provider = (me.make_provider)(&me.base);
                if let Err(e) = provider.revoke(&rt) {
                    log::warn!("could not revoke the session: {e}");
                }
            }
        });
    }

    /// A usable access token, refreshing first if it is at or near expiry.
    /// **Blocking** — worker threads only.
    ///
    /// Returns the token by value and the caller is expected to drop it: it goes
    /// to [`cloud::request`] and nowhere else. Nothing above this crate ever
    /// sees one.
    fn access_token(&self) -> Result<String, String> {
        let session = self.session().ok_or("nobody is signed in")?;
        let now = unix_now();
        if !session.needs_refresh(now) {
            return Ok(session.access_token);
        }
        let Some(refresh_token) = session.refresh_token.clone() else {
            // No refresh token and an expired access token: the session is over.
            // Say so rather than sending a token the server will reject.
            return Err("your sign-in expired — sign in again".into());
        };
        // One refresh at a time. Whoever waits here re-reads the session after,
        // and normally finds it already renewed.
        let _guard = self.refreshing.lock().map_err(|_| "the account lock is poisoned")?;
        if let Some(fresh) = self.session()
            && !fresh.needs_refresh(now)
        {
            return Ok(fresh.access_token);
        }
        let provider = (self.make_provider)(&self.base);
        match provider.refresh(&refresh_token) {
            Ok(tokens) => {
                let mut updated = session;
                updated.access_token = tokens.access_token.clone();
                if tokens.refresh_token.is_some() {
                    updated.refresh_token = tokens.refresh_token;
                }
                if let Err(e) = self.store.save(&updated) {
                    log::warn!("could not store the refreshed session: {e}");
                }
                if let Ok(mut i) = self.inner.lock() {
                    i.session = Some(updated);
                }
                Ok(tokens.access_token)
            }
            // The refresh token is dead — the session is unrecoverable, so end
            // it here rather than failing every call from now on with a riddle.
            Err(RefreshError::Invalid) => {
                if let Ok(mut i) = self.inner.lock() {
                    i.session = None;
                    i.phase = Phase::SignedOut;
                }
                let _ = self.store.clear();
                Err("your sign-in expired — sign in again".into())
            }
            // A network blip must not sign anyone out.
            Err(RefreshError::Transient(e)) => Err(e),
        }
    }

    /// Send an authorized request to the Cloud API on a worker thread and post
    /// `(id, reply)` back through `tx`. Never blocks the caller.
    pub fn request(
        &self,
        id: u64,
        method: &str,
        path: &str,
        body: Option<String>,
        timeout: Duration,
        tx: Sender<(u64, CloudReply)>,
    ) -> Result<(), String> {
        let me = self.clone();
        let method = method.to_string();
        let path = path.to_string();
        detach("floptle-account-request", move || {
            let reply = match me.access_token() {
                #[cfg(not(target_arch = "wasm32"))]
                Ok(token) => cloud::request(&me.base, &token, &method, &path, body, timeout),
                // A browser cannot reach the Cloud API, and the reason has
                // nothing to do with having a token — see
                // `auth::OfflineProvider`. Reply with it rather than leaving the
                // script's callback pending forever.
                #[cfg(target_arch = "wasm32")]
                Ok(_token) => {
                    let _ = (&me.base, &method, &path, body, timeout);
                    CloudReply::failed(auth::NO_WEB_AUTH)
                }
                Err(e) => CloudReply::failed(e),
            };
            // The receiver is gone only when the host itself has.
            let _ = tx.send((id, reply));
        })
            .map_err(|e| format!("could not start a worker: {e}"))
    }
}

/// Seconds since the Unix epoch, from a clock that exists on every target —
/// `std::time::SystemTime::now()` compiles for a page and panics there.
fn unix_now() -> u64 {
    #[cfg(not(target_arch = "wasm32"))]
    use std::time::{SystemTime, UNIX_EPOCH};
    #[cfg(target_arch = "wasm32")]
    use web_time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{DeviceCode, PollOutcome, Tokens, UserInfo};
    use std::sync::mpsc::channel;

    /// An in-memory store, the same shape the Hub's tests use.
    #[derive(Default)]
    struct MemStore(Mutex<Option<Session>>);
    impl TokenStore for MemStore {
        fn save(&self, s: &Session) -> Result<(), String> {
            *self.0.lock().unwrap() = Some(s.clone());
            Ok(())
        }
        fn load(&self) -> Option<Session> {
            self.0.lock().unwrap().clone()
        }
        fn clear(&self) -> Result<(), String> {
            *self.0.lock().unwrap() = None;
            Ok(())
        }
    }

    /// Approves on the Nth poll, and records how the flow used it.
    struct FakeProvider {
        polls_until_grant: Mutex<u32>,
        refresh_result: bool,
    }
    impl Provider for FakeProvider {
        fn start_device(&self, challenge: &str) -> Result<DeviceCode, String> {
            assert!(!challenge.is_empty(), "PKCE challenge must be sent");
            Ok(DeviceCode {
                device_code: "dev".into(),
                user_code: "WXYZ-9999".into(),
                verification_uri: "https://fopull.com/activate".into(),
                verification_uri_complete: Some("https://fopull.com/activate?code=WXYZ-9999".into()),
                expires_in: 900,
                interval: 1,
            })
        }
        fn poll_token(&self, _d: &str, _v: &str) -> Result<PollOutcome, String> {
            let mut n = self.polls_until_grant.lock().unwrap();
            if *n == 0 {
                Ok(PollOutcome::Granted(Tokens {
                    access_token: "at".into(),
                    refresh_token: Some("rt".into()),
                    scope: Some("openid profile cloud".into()),
                }))
            } else {
                *n -= 1;
                Ok(PollOutcome::Pending)
            }
        }
        fn refresh(&self, _t: &str) -> Result<Tokens, RefreshError> {
            if self.refresh_result {
                Ok(Tokens { access_token: "at2".into(), refresh_token: Some("rt2".into()), scope: None })
            } else {
                Err(RefreshError::Invalid)
            }
        }
        fn revoke(&self, _t: &str) -> Result<(), String> {
            Ok(())
        }
        fn userinfo(&self, _t: &str) -> Result<UserInfo, String> {
            Ok(UserInfo { sub: "u-1".into(), email: Some("ty@fopull.com".into()), name: Some("Ty".into()) })
        }
        fn entitlements(&self, _t: &str) -> Result<Entitlements, String> {
            Ok(Entitlements { tier: "free".into() })
        }
    }

    fn account(store: Arc<MemStore>, polls: u32, refresh_ok: bool) -> Account {
        Account::with(
            "https://fopull.com",
            store,
            Arc::new(move |_| {
                Box::new(FakeProvider {
                    polls_until_grant: Mutex::new(polls),
                    refresh_result: refresh_ok,
                }) as Box<dyn Provider + Send>
            }),
        )
    }

    /// Spin until `f` is true or we give up, so a test never depends on a sleep
    /// being long enough on a loaded machine.
    fn until(mut f: impl FnMut() -> bool) -> bool {
        for _ in 0..400 {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn signing_in_shows_a_code_before_it_shows_a_player() {
        let store = Arc::new(MemStore::default());
        let a = account(store.clone(), 2, true);
        assert_eq!(a.phase(), Phase::SignedOut);
        a.sign_in();
        // The player is shown a code while the flow is still waiting — the whole
        // reason this is asynchronous.
        assert!(
            until(|| matches!(a.phase(), Phase::Waiting { .. })),
            "never reached Waiting, phase was {:?}",
            a.phase()
        );
        match a.phase() {
            Phase::Waiting { user_code, url, .. } => {
                assert_eq!(user_code, "WXYZ-9999");
                assert!(url.contains("activate"), "url was {url}");
            }
            other => panic!("expected Waiting, got {other:?}"),
        }
        assert!(until(|| a.phase() == Phase::SignedIn), "never signed in");
        let s = a.session().expect("a session");
        assert_eq!(s.sub, "u-1");
        assert_eq!(s.player_name(), "Ty");
        // …and it persisted, which is what makes the next launch free.
        assert_eq!(store.load().map(|s| s.sub), Some("u-1".into()));
    }

    #[test]
    fn a_second_sign_in_while_one_is_running_is_ignored() {
        // Two live user codes means a player typing the one that no longer works.
        let a = account(Arc::new(MemStore::default()), 50, true);
        a.sign_in();
        assert!(until(|| a.phase().is_busy()));
        let before = a.phase();
        a.sign_in();
        assert_eq!(a.phase(), before);
        a.cancel_sign_in();
        assert!(until(|| !a.phase().is_busy()));
    }

    #[test]
    fn cancelling_returns_to_where_it_started() {
        let a = account(Arc::new(MemStore::default()), 100, true);
        a.sign_in();
        assert!(until(|| matches!(a.phase(), Phase::Waiting { .. })));
        a.cancel_sign_in();
        assert_eq!(a.phase(), Phase::SignedOut);
        assert!(a.session().is_none());
    }

    #[test]
    fn a_restored_session_is_signed_in_without_any_network() {
        let store = Arc::new(MemStore::default());
        store
            .save(&Session {
                sub: "u-9".into(),
                name: Some("Someone".into()),
                email: None,
                tier: "free".into(),
                access_token: "stored".into(),
                refresh_token: Some("r".into()),
            })
            .unwrap();
        // A provider that would panic if touched: restoring must not call one.
        let a = Account::with(
            "https://fopull.com",
            store,
            Arc::new(|_| panic!("restore must not contact the provider")),
        );
        a.restore();
        assert!(until(|| a.phase() == Phase::SignedIn));
        assert_eq!(a.session().unwrap().player_name(), "Someone");
    }

    #[test]
    fn a_request_without_a_session_answers_instead_of_hanging() {
        let a = account(Arc::new(MemStore::default()), 0, true);
        let (tx, rx) = channel();
        a.request(7, "GET", "/wallet", None, Duration::from_secs(5), tx).unwrap();
        let (id, reply) = rx.recv_timeout(Duration::from_secs(5)).expect("a reply");
        assert_eq!(id, 7);
        assert_eq!(reply.status, 0);
        assert!(reply.error.unwrap().contains("signed in"));
    }

    #[test]
    fn a_dead_refresh_token_ends_the_session_rather_than_failing_forever() {
        // An opaque (non-JWT) access token always reads as "needs refresh", so
        // the very first request goes down the refresh path.
        let store = Arc::new(MemStore::default());
        store
            .save(&Session {
                sub: "u-1".into(),
                name: None,
                email: None,
                tier: "free".into(),
                access_token: "opaque".into(),
                refresh_token: Some("dead".into()),
            })
            .unwrap();
        let a = account(store.clone(), 0, false);
        a.restore();
        assert!(until(|| a.phase() == Phase::SignedIn));
        let (tx, rx) = channel();
        a.request(1, "GET", "/wallet", None, Duration::from_secs(5), tx).unwrap();
        let (_, reply) = rx.recv_timeout(Duration::from_secs(5)).expect("a reply");
        assert!(reply.error.unwrap().contains("expired"));
        // Signed out, locally and in the store — not left in a state where every
        // future call fails with the same message and nothing explains why.
        assert!(until(|| a.phase() == Phase::SignedOut));
        assert!(a.session().is_none());
        assert!(store.load().is_none());
    }

    /// Against the LIVE provider. Ignored so CI stays offline:
    /// `cargo test -p floptle-account -- --ignored live_`
    ///
    /// Only the half a machine can do alone — asking for a device code. It
    /// proves the endpoint, the PKCE challenge, the scope and the response
    /// shape; the approval needs a person and a browser, which is what Ty's
    /// run through Fofighter is for.
    #[test]
    #[ignore = "hits the live fopull.com provider"]
    fn live_the_provider_issues_a_device_code() {
        let provider = auth::HttpProvider::new(crate::DEFAULT_BASE);
        let pkce = auth::Pkce::generate();
        let dc = provider.start_device(&pkce.challenge).expect("a device code");
        assert!(!dc.user_code.is_empty(), "a user code to show the player");
        assert!(
            dc.approve_url().contains("fopull.com"),
            "the approval URL should be on fopull.com, got {}",
            dc.approve_url()
        );
        assert!(dc.interval >= 1 && dc.expires_in > 60, "sane polling hints: {dc:?}");
        // Not approved, so the first poll must say "still waiting" — the state
        // the whole flow is built around, and the one an unreachable or
        // misconfigured provider would fail to produce.
        assert!(
            matches!(
                provider.poll_token(&dc.device_code, &pkce.verifier),
                Ok(PollOutcome::Pending)
            ),
            "an unapproved device code should poll as Pending"
        );
    }

    #[test]
    fn signing_out_is_immediate_and_clears_the_store() {
        let store = Arc::new(MemStore::default());
        let a = account(store.clone(), 0, true);
        a.sign_in();
        assert!(until(|| a.phase() == Phase::SignedIn));
        a.sign_out();
        // Immediate: no waiting on the revoke round trip.
        assert_eq!(a.phase(), Phase::SignedOut);
        assert!(a.session().is_none());
        assert!(until(|| store.load().is_none()), "the store should be cleared");
    }
}
