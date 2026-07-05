//! Signing the Hub into fopull.com via the OAuth 2.0 **Device Authorization Grant**
//! (RFC 8628) with mandatory PKCE, per `floptle-platform/contracts/identity-auth.md`.
//!
//! The HTTP transport and the token store are behind traits so the flow is fully
//! unit-testable offline (a mock provider + in-memory store) and the real impls
//! ([`HttpProvider`] over `ureq`, [`KeyringStore`] over the OS keyring) drop in unchanged.
//! No password ever touches the Hub — the user approves in their browser.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// A PKCE (RFC 7636) pair: a random high-entropy `verifier` and its S256 `challenge`.
/// The Hub sends the challenge to `/oauth/device` and proves possession with the verifier
/// at `/oauth/token`, so a leaked `device_code` is useless without the verifier.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    pub fn generate() -> Self {
        let mut raw = [0u8; 32];
        getrandom::getrandom(&mut raw).expect("OS RNG unavailable");
        // A base64url verifier is all unreserved chars (43 chars for 32 bytes), so it's a
        // valid `code_verifier` as-is.
        let verifier = URL_SAFE_NO_PAD.encode(raw);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Self { verifier, challenge }
    }
}

// ---- wire types (subset of the contract we consume) ---------------------------------

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    /// Seconds the device/user code stays valid — a client-side deadline so polling can't
    /// outrun the server (RFC 8628 §3.2).
    #[serde(default = "default_expires_in")]
    pub expires_in: u64,
    #[serde(default = "default_interval")]
    pub interval: u64,
}

impl DeviceCode {
    /// The URL to send the user to — the complete one (pre-fills the code) if present.
    pub fn approve_url(&self) -> &str {
        self.verification_uri_complete.as_deref().unwrap_or(&self.verification_uri)
    }
}

fn default_interval() -> u64 {
    5
}
fn default_expires_in() -> u64 {
    900
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Tokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
pub struct UserInfo {
    pub sub: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Entitlements {
    #[serde(default)]
    pub tier: String,
}

#[derive(Deserialize)]
struct OauthError {
    #[serde(default)]
    error: String,
}

/// The result of one `/oauth/token` poll during the device flow.
#[derive(Debug, PartialEq)]
pub enum PollOutcome {
    /// Still waiting for the user to approve — keep polling.
    Pending,
    /// Polling too fast (or a transient upstream hiccup) — back off, keep polling.
    SlowDown,
    /// A transient upstream failure (gateway 5xx / rate-limit / network blip) with the device
    /// code still valid — back off and keep polling, don't abort.
    Transient,
    /// Approved.
    Granted(Tokens),
    /// The user (or server) explicitly rejected it — terminal.
    Denied(String),
    /// The device code expired before approval — terminal.
    Expired,
}

/// Why a refresh failed: a permanent rejection (the token is dead — sign the user out) vs a
/// transient error (keep the session; a network blip mustn't sign anyone out).
pub enum RefreshError {
    /// `invalid_grant` — the refresh token is revoked/expired; the session is unrecoverable.
    Invalid,
    /// A transient/unexpected failure — keep the existing session and try again later.
    Transient(String),
}

// ---- provider (the HTTP calls, behind a trait) --------------------------------------

/// The identity-provider calls the device flow needs. Real impl: [`HttpProvider`]; tests
/// use a mock.
pub trait Provider {
    fn start_device(&self, challenge: &str) -> Result<DeviceCode, String>;
    fn poll_token(&self, device_code: &str, verifier: &str) -> Result<PollOutcome, String>;
    fn refresh(&self, refresh_token: &str) -> Result<Tokens, RefreshError>;
    fn revoke(&self, refresh_token: &str) -> Result<(), String>;
    fn userinfo(&self, access_token: &str) -> Result<UserInfo, String>;
    fn entitlements(&self, access_token: &str) -> Result<Entitlements, String>;
}

/// The device-flow client for `floptle-hub` against a provider base URL (`https://fopull.com`,
/// or a dev instance). It only ever contacts `base`, so the access token never reaches any
/// other host — the transport itself is the scoping (see [`is_fopull_host`] for the future
/// Cloud-call path). Requests are timeout-bounded so a hung endpoint can't wedge the worker.
pub struct HttpProvider {
    base: String,
    client_id: String,
    agent: ureq::Agent,
}

impl HttpProvider {
    pub fn new(base: impl Into<String>) -> Self {
        let base = base.into().trim_end_matches('/').to_string();
        // Defence-in-depth: the token is about to be sent here, so warn loudly if the
        // configured base is neither fopull nor an obvious local dev target.
        if !is_fopull_host(&base) && !is_local_host(&base) {
            log::warn!("auth base URL {base} is not a fopull.com or localhost host");
        }
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(30))
            .timeout_write(Duration::from_secs(30))
            .build();
        Self { base, client_id: "floptle-hub".into(), agent }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.base)
    }
}

impl Provider for HttpProvider {
    fn start_device(&self, challenge: &str) -> Result<DeviceCode, String> {
        self.agent
            .post(&self.url("oauth/device"))
            .send_form(&[
                ("client_id", self.client_id.as_str()),
                ("scope", "openid profile cloud"),
                ("code_challenge", challenge),
                ("code_challenge_method", "S256"),
            ])
            .map_err(|e| format!("could not reach the sign-in server: {e}"))?
            .into_json()
            .map_err(|e| format!("unexpected device response: {e}"))
    }

    fn poll_token(&self, device_code: &str, verifier: &str) -> Result<PollOutcome, String> {
        match self.agent.post(&self.url("oauth/token")).send_form(&[
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", device_code),
            ("code_verifier", verifier),
            ("client_id", self.client_id.as_str()),
        ]) {
            Ok(resp) => resp
                .into_json::<Tokens>()
                .map(PollOutcome::Granted)
                .map_err(|e| format!("unexpected token response: {e}")),
            // The device grant signals "keep waiting" / "back off" / a real rejection as an
            // OAuth error body on a 4xx. A 5xx / 429 / unparseable body is a TRANSIENT upstream
            // failure while the device code is still valid — back off, don't abort.
            Err(ureq::Error::Status(code, resp)) => {
                let err = resp.into_json::<OauthError>().map(|e| e.error).unwrap_or_default();
                Ok(match err.as_str() {
                    "authorization_pending" => PollOutcome::Pending,
                    "slow_down" => PollOutcome::SlowDown,
                    "expired_token" => PollOutcome::Expired,
                    "access_denied" => PollOutcome::Denied("access_denied".into()),
                    _ if code >= 500 || code == 429 => PollOutcome::Transient,
                    "" => PollOutcome::Transient,
                    other => PollOutcome::Denied(other.into()),
                })
            }
            // Transport failure (DNS / reset / timeout): transient, keep polling.
            Err(ureq::Error::Transport(_)) => Ok(PollOutcome::Transient),
        }
    }

    fn refresh(&self, refresh_token: &str) -> Result<Tokens, RefreshError> {
        match self.agent.post(&self.url("oauth/token")).send_form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", self.client_id.as_str()),
        ]) {
            Ok(resp) => resp
                .into_json()
                .map_err(|e| RefreshError::Transient(format!("unexpected refresh response: {e}"))),
            Err(ureq::Error::Status(_, resp)) => {
                let err = resp.into_json::<OauthError>().map(|e| e.error).unwrap_or_default();
                // Only a definitive invalid_grant means the token is dead.
                if err == "invalid_grant" {
                    Err(RefreshError::Invalid)
                } else {
                    Err(RefreshError::Transient(if err.is_empty() { "refresh failed".into() } else { err }))
                }
            }
            Err(ureq::Error::Transport(e)) => Err(RefreshError::Transient(format!("could not refresh: {e}"))),
        }
    }

    fn revoke(&self, refresh_token: &str) -> Result<(), String> {
        self.agent
            .post(&self.url("oauth/revoke"))
            .send_form(&[("token", refresh_token), ("client_id", self.client_id.as_str())])
            .map(|_| ())
            .map_err(|e| format!("could not revoke the session: {e}"))
    }

    fn userinfo(&self, access_token: &str) -> Result<UserInfo, String> {
        self.agent
            .get(&self.url("userinfo"))
            .set("Authorization", &format!("Bearer {access_token}"))
            .call()
            .map_err(|e| format!("could not read your account: {e}"))?
            .into_json()
            .map_err(|e| format!("unexpected userinfo response: {e}"))
    }

    fn entitlements(&self, access_token: &str) -> Result<Entitlements, String> {
        self.agent
            .get(&self.url("entitlements"))
            .set("Authorization", &format!("Bearer {access_token}"))
            .call()
            .map_err(|e| format!("could not read your plan: {e}"))?
            .into_json()
            .map_err(|e| format!("unexpected entitlements response: {e}"))
    }
}

/// Widest gap between polls, so `slow_down`/transient back-off can't grow without bound.
const MAX_POLL_INTERVAL: u64 = 30;

/// Poll `/oauth/token` until the user approves (or it's denied/expired/cancelled), sleeping
/// `interval` seconds between polls and widening it on `slow_down`/transient errors (RFC 8628
/// §3.5). Bounded three ways so it can never wedge: a terminal outcome, the `cancel` flag, or
/// the `expires_in` client-side deadline. `sleep` is injected so the loop is testable without
/// real time; the Hub passes a real sleep that also observes `cancel`.
pub fn poll_until<P: Provider>(
    provider: &P,
    device_code: &str,
    verifier: &str,
    interval: u64,
    expires_in: u64,
    cancel: &AtomicBool,
    mut sleep: impl FnMut(u64),
) -> Result<Tokens, String> {
    let mut interval = interval.clamp(1, MAX_POLL_INTERVAL);
    let mut elapsed = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("sign-in cancelled".into());
        }
        if expires_in > 0 && elapsed >= expires_in {
            return Err("the sign-in code expired — try again".into());
        }
        sleep(interval);
        elapsed += interval;
        match provider.poll_token(device_code, verifier)? {
            PollOutcome::Pending => {}
            PollOutcome::SlowDown | PollOutcome::Transient => {
                interval = (interval + 5).min(MAX_POLL_INTERVAL);
            }
            PollOutcome::Granted(t) => return Ok(t),
            PollOutcome::Denied(e) => return Err(format!("sign-in was denied ({e})")),
            PollOutcome::Expired => return Err("the sign-in code expired — try again".into()),
        }
    }
}

/// The `exp` (unix seconds) claim of a JWT access token, read **without verifying the
/// signature** — the Hub only needs it to know when to refresh; the resource server (Cloud)
/// is what actually verifies. `None` if the token isn't a JWT or has no numeric `exp`.
pub fn access_token_expiry(jwt: &str) -> Option<u64> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    claims.get("exp")?.as_u64()
}

/// True when `url`'s host is fopull.com (or a subdomain) — the only host, besides a local dev
/// instance, that the access token may be attached to. Mirrors `releases::is_github_host`
/// (strips scheme, userinfo, and port) so a crafted URL can't smuggle the token elsewhere.
pub fn is_fopull_host(url: &str) -> bool {
    host_of(url).is_some_and(|h| h == "fopull.com" || h.ends_with(".fopull.com"))
}

/// True for an obvious local dev target (so a `localhost` dev provider doesn't trip the
/// [`HttpProvider::new`] warning).
pub fn is_local_host(url: &str) -> bool {
    host_of(url).is_some_and(|h| h == "localhost" || h == "127.0.0.1" || h == "[::1]")
}

fn host_of(url: &str) -> Option<String> {
    let after = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")).unwrap_or(url);
    let authority = after.split('/').next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("").split(':').next().unwrap_or("");
    // Hostnames are case-insensitive (and ASCII here) — normalize so `Fopull.COM` matches.
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

// ---- session + token store ----------------------------------------------------------

/// The signed-in state the Hub persists: the tokens plus the derived identity/plan shown in
/// the UI. Stored via a [`TokenStore`] (the OS keyring in production) — never in `hub.json`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Session {
    pub sub: String,
    pub email: Option<String>,
    pub tier: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
}

impl Session {
    pub fn from_parts(tokens: Tokens, who: UserInfo, ent: Entitlements) -> Self {
        Self {
            sub: who.sub,
            email: who.email,
            tier: if ent.tier.is_empty() { "free".into() } else { ent.tier },
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
        }
    }

    /// A human label for the account (email, else the subject id, else a generic fallback).
    pub fn display_name(&self) -> &str {
        match self.email.as_deref() {
            Some(e) if !e.is_empty() => e,
            _ if !self.sub.is_empty() => self.sub.as_str(),
            _ => "your account",
        }
    }

    /// The access token is at/near expiry (or unreadable) and should be refreshed. A 60s
    /// skew avoids using a token that expires mid-request.
    pub fn needs_refresh(&self, now_unix: u64) -> bool {
        match access_token_expiry(&self.access_token) {
            Some(exp) => now_unix + 60 >= exp,
            None => true,
        }
    }
}

/// Where the signed-in [`Session`] is persisted. Production uses [`KeyringStore`]; tests use
/// an in-memory store.
pub trait TokenStore {
    fn save(&self, session: &Session) -> Result<(), String>;
    fn load(&self) -> Option<Session>;
    fn clear(&self) -> Result<(), String>;
}

/// Persists the session in the OS keyring (Keychain / Credential Manager / Secret Service)
/// as one JSON blob — the tokens never hit disk in plaintext. Cheap to construct (two owned
/// strings), so worker threads build their own rather than doing keyring I/O on the UI thread.
pub struct KeyringStore {
    service: String,
    user: String,
}

impl Default for KeyringStore {
    fn default() -> Self {
        Self { service: "com.fopull.floptle-hub".into(), user: "session".into() }
    }
}

impl KeyringStore {
    fn entry(&self) -> Result<keyring::Entry, String> {
        keyring::Entry::new(&self.service, &self.user).map_err(|e| format!("keyring: {e}"))
    }
}

impl TokenStore for KeyringStore {
    fn save(&self, session: &Session) -> Result<(), String> {
        let json = serde_json::to_string(session).map_err(|e| e.to_string())?;
        self.entry()?.set_password(&json).map_err(|e| format!("keyring save: {e}"))
    }

    fn load(&self) -> Option<Session> {
        let json = self.entry().ok()?.get_password().ok()?;
        serde_json::from_str(&json).ok()
    }

    fn clear(&self) -> Result<(), String> {
        match self.entry()?.delete_credential() {
            Ok(()) => Ok(()),
            // Nothing stored is a successful "cleared" outcome, not an error.
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!("keyring clear: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let p = Pkce::generate();
        // 32 random bytes → 43-char base64url verifier, all unreserved.
        assert_eq!(p.verifier.len(), 43);
        assert!(p.verifier.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        let expect = URL_SAFE_NO_PAD.encode(Sha256::digest(p.verifier.as_bytes()));
        assert_eq!(p.challenge, expect);
        // Fresh entropy each call.
        assert_ne!(p.verifier, Pkce::generate().verifier);
    }

    /// A scripted provider: returns the queued poll outcomes in order (then `Pending` once the
    /// queue drains), and rejects a wrong `code_verifier` (proving PKCE is exercised).
    struct MockProvider {
        outcomes: RefCell<Vec<PollOutcome>>,
        expect_verifier: String,
    }
    impl Provider for MockProvider {
        fn start_device(&self, _challenge: &str) -> Result<DeviceCode, String> {
            Ok(DeviceCode {
                device_code: "dev".into(),
                user_code: "ABCD-1234".into(),
                verification_uri: "https://fopull.com/activate".into(),
                verification_uri_complete: Some("https://fopull.com/activate?code=ABCD-1234".into()),
                expires_in: 900,
                interval: 5,
            })
        }
        fn poll_token(&self, _dc: &str, verifier: &str) -> Result<PollOutcome, String> {
            if verifier != self.expect_verifier {
                return Ok(PollOutcome::Denied("bad_verifier".into()));
            }
            let mut o = self.outcomes.borrow_mut();
            Ok(if o.is_empty() { PollOutcome::Pending } else { o.remove(0) })
        }
        fn refresh(&self, _t: &str) -> Result<Tokens, RefreshError> {
            Ok(Tokens { access_token: "new".into(), refresh_token: Some("r2".into()), scope: None })
        }
        fn revoke(&self, _t: &str) -> Result<(), String> {
            Ok(())
        }
        fn userinfo(&self, _t: &str) -> Result<UserInfo, String> {
            Ok(UserInfo { sub: "u1".into(), email: Some("ty@fopull.com".into()), name: None })
        }
        fn entitlements(&self, _t: &str) -> Result<Entitlements, String> {
            Ok(Entitlements { tier: "indie".into() })
        }
    }

    fn mock(outcomes: Vec<PollOutcome>) -> MockProvider {
        MockProvider { outcomes: RefCell::new(outcomes), expect_verifier: "v".into() }
    }
    fn granted() -> PollOutcome {
        PollOutcome::Granted(Tokens { access_token: "a".into(), refresh_token: Some("r".into()), scope: None })
    }
    fn no_cancel() -> AtomicBool {
        AtomicBool::new(false)
    }

    #[test]
    fn poll_backs_off_then_grants() {
        let p = mock(vec![PollOutcome::Pending, PollOutcome::SlowDown, granted()]);
        let mut slept = Vec::new();
        let tokens = poll_until(&p, "dev", "v", 5, 900, &no_cancel(), |s| slept.push(s)).unwrap();
        assert_eq!(tokens.access_token, "a");
        // 5 (pending) → 5 (slow_down, then +5) → 10 (granted).
        assert_eq!(slept, vec![5, 5, 10]);
    }

    #[test]
    fn transient_errors_keep_polling() {
        // A gateway blip (Transient) must NOT abort — it backs off and keeps going.
        let p = mock(vec![PollOutcome::Transient, granted()]);
        let mut slept = Vec::new();
        let tokens = poll_until(&p, "dev", "v", 5, 900, &no_cancel(), |s| slept.push(s)).unwrap();
        assert_eq!(tokens.access_token, "a");
        assert_eq!(slept, vec![5, 10]);
    }

    #[test]
    fn poll_reports_denied_and_expired() {
        let denied = mock(vec![PollOutcome::Denied("access_denied".into())]);
        assert!(poll_until(&denied, "dev", "v", 5, 900, &no_cancel(), |_| {}).is_err());
        let expired = mock(vec![PollOutcome::Expired]);
        assert!(poll_until(&expired, "dev", "v", 5, 900, &no_cancel(), |_| {}).is_err());
    }

    #[test]
    fn poll_honors_deadline_and_cancel() {
        // Never approved: the client-side expiry ends it even if the server never says so.
        let never = mock(vec![]); // drains → always Pending
        let err = poll_until(&never, "dev", "v", 5, 12, &no_cancel(), |_| {}).unwrap_err();
        assert!(err.contains("expired"), "got {err}");
        // A set cancel flag ends it immediately.
        let cancel = AtomicBool::new(true);
        assert!(poll_until(&mock(vec![granted()]), "dev", "v", 5, 900, &cancel, |_| {}).is_err());
    }

    #[test]
    fn wrong_verifier_is_rejected() {
        let p = MockProvider { outcomes: RefCell::new(vec![granted()]), expect_verifier: "correct".into() };
        // A mismatched verifier never yields tokens — PKCE binding is enforced.
        assert!(poll_until(&p, "dev", "WRONG", 5, 900, &no_cancel(), |_| {}).is_err());
    }

    #[test]
    fn access_token_expiry_reads_exp() {
        // header.payload.sig with payload {"exp":1893456000}
        let payload = URL_SAFE_NO_PAD.encode(br#"{"sub":"u1","exp":1893456000}"#);
        let jwt = format!("h.{payload}.s");
        assert_eq!(access_token_expiry(&jwt), Some(1893456000));
        assert_eq!(access_token_expiry("not-a-jwt"), None);
    }

    #[test]
    fn session_needs_refresh_near_expiry() {
        let payload = URL_SAFE_NO_PAD.encode(br#"{"exp":1000}"#);
        let s = Session {
            sub: "u1".into(),
            email: None,
            tier: "free".into(),
            access_token: format!("h.{payload}.s"),
            refresh_token: Some("r".into()),
        };
        assert!(!s.needs_refresh(900)); // 900+60 < 1000
        assert!(s.needs_refresh(950)); // 950+60 >= 1000
        // An unreadable token always needs refresh.
        let bad = Session { access_token: "opaque".into(), ..s };
        assert!(bad.needs_refresh(0));
    }

    #[test]
    fn fopull_host_scoping_matches_github_style() {
        assert!(is_fopull_host("https://fopull.com/oauth/token"));
        assert!(is_fopull_host("https://dev.fopull.com/userinfo"));
        // Case-insensitive host match.
        assert!(is_fopull_host("https://Dev.Fopull.COM/userinfo"));
        assert!(!is_fopull_host("https://evil.com/"));
        // userinfo-smuggling: the real host is evil.com, not fopull.com.
        assert!(!is_fopull_host("https://fopull.com@evil.com/"));
        assert!(is_local_host("http://localhost:8000"));
        assert!(is_local_host("http://127.0.0.1:8000/oauth/device"));
    }

    #[test]
    fn session_display_name_falls_back() {
        let mut s = Session::from_parts(
            Tokens { access_token: "a".into(), refresh_token: None, scope: None },
            UserInfo { sub: "u1".into(), email: Some("e@x.com".into()), name: None },
            Entitlements::default(),
        );
        assert_eq!(s.tier, "free");
        assert_eq!(s.display_name(), "e@x.com");
        s.email = None;
        assert_eq!(s.display_name(), "u1");
        s.sub = String::new();
        assert_eq!(s.display_name(), "your account");
    }
}
