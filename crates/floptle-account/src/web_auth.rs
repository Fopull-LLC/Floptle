//! Signing in **from a browser page** — OAuth 2.0 authorization code with
//! mandatory S256 PKCE, per `floptle-platform/contracts/identity-auth.md` §6.
//!
//! This is a second client beside the device flow in [`crate::auth`], not a
//! replacement: §1–§5 and the Hub are untouched. A page is exactly the thing the
//! device grant exists to avoid needing — it can redirect — so it uses the flow
//! built for that, under its own client id, with its own shorter token
//! lifetimes (§6.5, chosen because `WebStore` is `localStorage` and every script
//! on the origin can read it).
//!
//! **Everything in this module except the last section is platform-independent
//! on purpose.** The protocol — building the authorize URL, generating and
//! checking `state`, reading the redirect, forming the token request, reading
//! the answer — is pure string work, so it compiles and is *tested* on the
//! desktop where the test suite actually runs. Only `fetch` and `location` are
//! browser-only, and they are a thin layer at the bottom that does no deciding.
//! A protocol bug found by a desktop unit test is one that never has to be found
//! in a tab.

use serde::{Deserialize, Serialize};

use crate::auth::{Pkce, Tokens};

/// The client id §6.1 requires.
///
/// **It must be sent on every call.** `client_id` is optional on `/oauth/token`
/// and still defaults to `floptle-hub`, so omitting it does not fail loudly — it
/// resolves to the Hub, whose grants a page is then refused with
/// `unauthorized_client`. Every request built here carries it.
pub const WEB_CLIENT_ID: &str = "floptle-web";

/// What a page asks for. Same set the desktop asks for.
const SCOPE: &str = "openid profile cloud";

/// What [`WebClient::begin`] produces and the redirect needs back.
///
/// The page stashes this before navigating and reads it when it returns. It
/// holds a secret (`verifier`), so it lives wherever the session lives and is
/// cleared the moment it has been spent.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Handshake {
    pub verifier: String,
    pub state: String,
    /// Echoed back at the token endpoint, where it is re-checked — it binds the
    /// code to the place it was issued to land (§6.2).
    pub redirect_uri: String,
}

/// Why a redirect did not yield a code.
#[derive(Debug, Clone, PartialEq)]
pub enum RedirectError {
    /// Neither `code` nor `error` — an ordinary page load, not a return from
    /// sign-in. The commonest case by far, and not a failure.
    NotARedirect,
    /// A redirect arrived but nothing was stashed: a stale link, a cleared
    /// store, or a different browser. The code cannot be spent without the
    /// verifier, so this is terminal rather than retryable.
    NoHandshake,
    /// **`state` did not match, so the code was NOT spent.** The one check that
    /// has to happen before the code is worth anything.
    StateMismatch,
    /// The provider refused, with its own reason.
    Denied(String),
}

impl RedirectError {
    /// What to show a player. `NotARedirect` has no message because it is not a
    /// failure — callers match on it rather than printing it.
    pub fn message(&self) -> String {
        match self {
            Self::NotARedirect => String::new(),
            Self::NoHandshake => "this sign-in link is stale — start signing in again".into(),
            Self::StateMismatch => {
                "the sign-in reply did not match the request this page made, so it was \
                 refused. Start signing in again."
                    .into()
            }
            Self::Denied(e) => match e.as_str() {
                "access_denied" => "sign-in was cancelled".into(),
                "invalid_scope" => {
                    "this build asked for a permission the account cannot grant".into()
                }
                other => format!("the sign-in server refused: {other}"),
            },
        }
    }
}

/// The browser sign-in client for one game, at one `redirect_uri`.
///
/// `redirect_uri` is the page that actually **receives the code** — a launcher
/// that redirects before the game loads, or a small popup page that hands it
/// back. Which of the two a build uses is the host page's decision and does not
/// change anything here; what matters is that this value is byte-for-byte the
/// URI registered against the game (§6.4), because it is matched exactly at
/// `/oauth/authorize` and then re-checked at `/oauth/token`.
pub struct WebClient {
    base: String,
    redirect_uri: String,
}

impl WebClient {
    pub fn new(base: impl Into<String>, redirect_uri: impl Into<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            redirect_uri: redirect_uri.into(),
        }
    }

    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    pub fn token_url(&self) -> String {
        format!("{}/oauth/token", self.base)
    }

    pub fn userinfo_url(&self) -> String {
        format!("{}/userinfo", self.base)
    }

    pub fn entitlements_url(&self) -> String {
        format!("{}/entitlements", self.base)
    }

    /// Step 1: the URL to send the player to, and the handshake to keep.
    ///
    /// The URL is a **navigation**, not a fetch — `/oauth/authorize` answers no
    /// CORS and needs none (§6.3).
    pub fn begin(&self) -> (String, Handshake) {
        let pkce = Pkce::generate();
        // §6.2 requires at least 32 characters and calls it deliberately stricter
        // than RFC 6749. 32 bytes of CSPRNG is 43 base64url characters, from the
        // same generator that just made the verifier.
        let state = Pkce::generate().verifier;
        let url = format!(
            "{}/oauth/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}\
             &state={}&code_challenge={}&code_challenge_method=S256",
            self.base,
            WEB_CLIENT_ID,
            percent_encode(&self.redirect_uri),
            percent_encode(SCOPE),
            percent_encode(&state),
            percent_encode(&pkce.challenge),
        );
        (
            url,
            Handshake {
                verifier: pkce.verifier,
                state,
                redirect_uri: self.redirect_uri.clone(),
            },
        )
    }

    /// Step 2: the form body that spends `code`.
    ///
    /// `redirect_uri` comes from the handshake rather than from `self` on
    /// purpose: it must equal the one the code was issued for, and if a build
    /// ever changed its redirect between the navigation and the return, sending
    /// today's would fail in a way that reads as a server bug.
    pub fn exchange_form(&self, code: &str, hs: &Handshake) -> String {
        form(&[
            ("grant_type", "authorization_code"),
            ("client_id", WEB_CLIENT_ID),
            ("code", code),
            ("redirect_uri", &hs.redirect_uri),
            ("code_verifier", &hs.verifier),
        ])
    }

    /// The form body that refreshes. The answer carries a **new** refresh token;
    /// store it and discard the old one — presenting a rotated token after the
    /// chain has moved on revokes the whole session (§6.2).
    pub fn refresh_form(&self, refresh_token: &str) -> String {
        form(&[
            ("grant_type", "refresh_token"),
            ("client_id", WEB_CLIENT_ID),
            ("refresh_token", refresh_token),
        ])
    }
}

/// Read the query a redirect came back with.
///
/// **The `state` check happens before the code is returned**, which is the whole
/// point of `state` — a code delivered to this page by somebody else's request
/// is refused unspent. Order matters and is deliberate: a query carrying neither
/// a code nor an error is an ordinary page load and must not be reported as a
/// failure, but once it is a redirect, a missing or mismatched `state` outranks
/// even the provider's own `error`, because a forged error is as much a forgery
/// as a forged code.
pub fn read_redirect(query: &str, stashed: Option<&Handshake>) -> Result<String, RedirectError> {
    let params = parse_query(query);
    let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());

    let code = get("code");
    let error = get("error");
    if code.is_none() && error.is_none() {
        return Err(RedirectError::NotARedirect);
    }
    let Some(hs) = stashed else {
        return Err(RedirectError::NoHandshake);
    };
    if get("state").as_deref() != Some(hs.state.as_str()) {
        return Err(RedirectError::StateMismatch);
    }
    if let Some(e) = error {
        return Err(RedirectError::Denied(e));
    }
    // `code.is_none() && error.is_none()` returned above, and `error` was just
    // consumed, so this is a code.
    code.ok_or(RedirectError::NotARedirect)
}

/// What `/oauth/token` said.
///
/// A non-2xx carries an OAuth error body, and `error_description` is the half
/// worth showing — `invalid_grant` alone tells a developer nothing about which
/// of the five things that can be wrong actually was.
pub fn parse_token_response(status: u16, body: &str) -> Result<Tokens, String> {
    if (200..300).contains(&status) {
        return serde_json::from_str::<Tokens>(body)
            .map_err(|e| format!("the sign-in server sent a token reply this build could not read: {e}"));
    }
    #[derive(Deserialize, Default)]
    struct Err_ {
        #[serde(default)]
        error: String,
        #[serde(default)]
        error_description: String,
    }
    let e: Err_ = serde_json::from_str(body).unwrap_or_default();
    Err(match (e.error.as_str(), e.error_description.as_str()) {
        ("", "") => format!("the sign-in server answered {status}"),
        (err, "") => format!("the sign-in server refused: {err}"),
        (err, desc) => format!("the sign-in server refused: {err} — {desc}"),
    })
}

// ---- small pure helpers -------------------------------------------------------------

/// Percent-encode for a query value: everything outside RFC 3986's unreserved
/// set. Written here rather than pulled in, because the whole need is one rule
/// and a dependency for it would be in every build of every platform.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Split `a=1&b=2`, percent-decoding values. Accepts a leading `?` so a caller
/// can hand `location.search` straight in.
fn parse_query(q: &str) -> Vec<(String, String)> {
    q.trim_start_matches('?')
        .split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            Some((percent_decode(k), percent_decode(v)))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            // `+` is a space in a query string, which is NOT the same rule as in
            // a path — and an OAuth `error_description` is exactly the field
            // that arrives with spaces in it.
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
                {
                    Ok(v) => {
                        out.push(v);
                        i += 3;
                    }
                    // A stray `%` is a literal `%`, not a reason to lose the rest
                    // of the value.
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> WebClient {
        WebClient::new("https://fopull.com/", "https://orbit-racer.example.com/auth.html")
    }

    /// **The client id must be on every request.** `client_id` is optional at
    /// `/oauth/token` and defaults to `floptle-hub`, so leaving it off does not
    /// fail as a missing parameter — it silently resolves to the Hub and is then
    /// refused the grant. That is the exact silent-wrong-answer shape this
    /// engine's bug ledger is full of, so it is pinned on all three requests.
    #[test]
    fn every_request_names_the_web_client() {
        let c = client();
        let (url, hs) = c.begin();
        assert!(url.contains("client_id=floptle-web"), "{url}");
        assert!(c.exchange_form("abc", &hs).contains("client_id=floptle-web"));
        assert!(c.refresh_form("rt").contains("client_id=floptle-web"));
    }

    /// §6.2 requires `state` of at least 32 characters and PKCE's S256 challenge.
    /// Both come from the CSPRNG, and no two sign-ins may share either.
    #[test]
    fn the_handshake_meets_the_contracts_minimums_and_never_repeats() {
        let c = client();
        let (url, a) = c.begin();
        let (_, b) = c.begin();
        assert!(a.state.len() >= 32, "state is {} chars, contract wants 32", a.state.len());
        assert_ne!(a.state, b.state, "two sign-ins shared a state");
        assert_ne!(a.verifier, b.verifier, "two sign-ins shared a verifier");
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("response_type=code"));
    }

    /// The redirect URI is matched byte for byte at the provider, so it has to
    /// survive the round trip through the query string intact.
    #[test]
    fn the_redirect_uri_survives_the_query_string_exactly() {
        let c = client();
        let (url, hs) = c.begin();
        assert!(
            url.contains("redirect_uri=https%3A%2F%2Forbit-racer.example.com%2Fauth.html"),
            "{url}"
        );
        let body = c.exchange_form("abc", &hs);
        let round = parse_query(&body);
        let sent = round.iter().find(|(k, _)| k == "redirect_uri").unwrap();
        assert_eq!(sent.1, "https://orbit-racer.example.com/auth.html");
    }

    /// **A code delivered with the wrong `state` must not be spent.** This is the
    /// single check that stands between a page and somebody else's authorization
    /// code being exchanged by it.
    #[test]
    fn a_code_with_the_wrong_state_is_refused_unspent() {
        let (_, hs) = client().begin();
        let q = format!("?code=THE_CODE&state={}", "not-the-state-we-sent");
        assert_eq!(read_redirect(&q, Some(&hs)), Err(RedirectError::StateMismatch));
    }

    /// A missing `state` is a mismatch, not a pass. An attacker omits what they
    /// cannot forge, so "absent" must never be the lenient case.
    #[test]
    fn a_code_with_no_state_at_all_is_refused_too() {
        let (_, hs) = client().begin();
        assert_eq!(read_redirect("?code=THE_CODE", Some(&hs)), Err(RedirectError::StateMismatch));
    }

    /// The good path, and the only one that yields a code.
    #[test]
    fn a_matching_state_yields_the_code() {
        let (_, hs) = client().begin();
        let q = format!("?code=THE_CODE&state={}", hs.state);
        assert_eq!(read_redirect(&q, Some(&hs)), Ok("THE_CODE".to_string()));
    }

    /// **An ordinary page load is not a failure.** Every boot runs this, and a
    /// game that reported an auth error on each of them would be worse than one
    /// that never signed in.
    #[test]
    fn an_ordinary_page_load_is_not_a_sign_in_failure() {
        assert_eq!(read_redirect("", None), Err(RedirectError::NotARedirect));
        assert_eq!(read_redirect("?level=3&debug=1", None), Err(RedirectError::NotARedirect));
    }

    /// A forged `error` is as much a forgery as a forged code, so `state`
    /// outranks it. Otherwise anybody could push a page into a failed-sign-in
    /// state by handing it a link.
    #[test]
    fn state_is_checked_even_on_an_error_redirect() {
        let (_, hs) = client().begin();
        assert_eq!(
            read_redirect("?error=access_denied&state=forged", Some(&hs)),
            Err(RedirectError::StateMismatch)
        );
        let ok = format!("?error=access_denied&state={}", hs.state);
        assert_eq!(read_redirect(&ok, Some(&hs)), Err(RedirectError::Denied("access_denied".into())));
    }

    /// A redirect with nothing stashed cannot be completed — there is no
    /// verifier — so it is terminal and says so rather than looking retryable.
    #[test]
    fn a_redirect_with_no_handshake_is_terminal() {
        let e = read_redirect("?code=x&state=y", None).unwrap_err();
        assert_eq!(e, RedirectError::NoHandshake);
        assert!(e.message().contains("stale"), "{}", e.message());
    }

    /// Cancelling is a sentence a player can read, not an OAuth constant.
    #[test]
    fn a_cancelled_sign_in_reads_as_cancelled() {
        assert_eq!(RedirectError::Denied("access_denied".into()).message(), "sign-in was cancelled");
        // An error nobody anticipated still names itself rather than vanishing.
        assert!(RedirectError::Denied("teapot".into()).message().contains("teapot"));
    }

    /// **`error_description` is the half worth showing.** `invalid_grant` alone
    /// tells a developer nothing about which of the several things it covers
    /// actually went wrong.
    #[test]
    fn a_refused_exchange_keeps_the_servers_description() {
        let msg = parse_token_response(
            400,
            r#"{"error":"invalid_grant","error_description":"code already redeemed"}"#,
        )
        .unwrap_err();
        assert!(msg.contains("invalid_grant"), "{msg}");
        assert!(msg.contains("code already redeemed"), "{msg}");

        // A body that is not the shape we expect still produces the status.
        assert!(parse_token_response(502, "<html>bad gateway</html>").unwrap_err().contains("502"));
    }

    #[test]
    fn a_good_exchange_yields_the_tokens() {
        let t = parse_token_response(
            200,
            r#"{"access_token":"AT","refresh_token":"RT","scope":"openid profile cloud","expires_in":900}"#,
        )
        .expect("should parse");
        assert_eq!(t.access_token, "AT");
        assert_eq!(t.refresh_token.as_deref(), Some("RT"));
    }

    /// `+` is a space in a query string — and `error_description` is exactly the
    /// field that comes back with spaces in it, so decoding it as a literal `+`
    /// garbles the one message a developer reads.
    #[test]
    fn query_decoding_handles_spaces_and_a_stray_percent() {
        let q = parse_query("?a=one+two&b=three%20four&c=100%&d=%2Fpath");
        let get = |k: &str| q.iter().find(|(n, _)| n == k).unwrap().1.clone();
        assert_eq!(get("a"), "one two");
        assert_eq!(get("b"), "three four");
        assert_eq!(get("c"), "100%", "a stray % must not eat the value");
        assert_eq!(get("d"), "/path");
    }
}

// ---- the browser layer: `fetch` and `location`, and nothing that decides ------------
//
// Everything above this line is pure and tested on the desktop. Everything below
// is I/O against APIs that only exist in a page, and is deliberately as thin as
// it can be — it moves strings, it does not make judgements. The judgements are
// all above, where a test can reach them.

/// Reading and writing the page's own state: the stashed handshake, the query it
/// came back with, and the address bar.
#[cfg(target_arch = "wasm32")]
pub mod browser {
    use super::{Handshake, RedirectError, WebClient};
    use crate::auth::{Session, Tokens};
    use wasm_bindgen::JsCast as _;

    /// Where the handshake waits while the player is away at fopull.com. It
    /// holds the PKCE verifier, so it is cleared the moment it is spent —
    /// including when the exchange FAILS, because a code cannot be spent twice
    /// and a verifier kept past its code is a secret with no purpose.
    const STASH_KEY: &str = "com.fopull.floptle.pkce";

    fn storage() -> Result<web_sys::Storage, String> {
        web_sys::window()
            .ok_or_else(|| "no window: this is not a browser page".to_string())?
            .local_storage()
            .map_err(|_| "the browser refused local storage (site data may be blocked)".to_string())?
            .ok_or_else(|| "this browser has no local storage".to_string())
    }

    pub fn stash(hs: &Handshake) -> Result<(), String> {
        let json = serde_json::to_string(hs).map_err(|e| e.to_string())?;
        storage()?.set_item(STASH_KEY, &json).map_err(|_| "could not stash the sign-in".to_string())
    }

    /// Read the handshake AND remove it in one go: a stash that survives its own
    /// redirect is a replay window.
    pub fn take_stash() -> Option<Handshake> {
        let s = storage().ok()?;
        let json = s.get_item(STASH_KEY).ok()??;
        let _ = s.remove_item(STASH_KEY);
        serde_json::from_str(&json).ok()
    }

    /// `location.search`, or empty if there is no page.
    pub fn current_query() -> String {
        web_sys::window()
            .and_then(|w| w.location().search().ok())
            .unwrap_or_default()
    }

    /// The page's own address with no query or fragment — the default
    /// `redirect_uri`, and the thing that has to be registered against the game.
    pub fn current_page_url() -> Result<String, String> {
        let loc = web_sys::window().ok_or_else(|| "no window".to_string())?.location();
        let origin = loc.origin().map_err(|_| "no origin".to_string())?;
        let path = loc.pathname().map_err(|_| "no path".to_string())?;
        Ok(format!("{origin}{path}"))
    }

    /// Take `?code=&state=` out of the address bar without reloading.
    ///
    /// **A spent authorization code must not stay in the URL**: it lands in
    /// history, in the `Referer` of the next request the page makes, and in the
    /// link a player copies to a friend.
    pub fn scrub_query() {
        let Some(win) = web_sys::window() else { return };
        let Ok(url) = current_page_url() else { return };
        if let Ok(h) = win.history() {
            let _ = h.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&url));
        }
    }

    /// Send the player to the consent screen by navigating this page away.
    ///
    /// **This tears the game down** — the wasm module, the GPU device and all
    /// unsaved state go with the page, and the game starts again from scratch
    /// when the player returns. That is why sign-in belongs at a menu and not
    /// mid-play, and it is the shape that works everywhere, including inside a
    /// sandboxed iframe where a popup would be blocked outright.
    pub fn navigate(url: &str) -> Result<(), String> {
        web_sys::window()
            .ok_or_else(|| "no window".to_string())?
            .location()
            .assign(url)
            .map_err(|_| "the browser refused to open the sign-in page".to_string())
    }

    async fn send(req: web_sys::Request) -> Result<(u16, String), String> {
        let win = web_sys::window().ok_or_else(|| "no window".to_string())?;
        let resp: web_sys::Response = wasm_bindgen_futures::JsFuture::from(win.fetch_with_request(&req))
            .await
            .map_err(|_| {
                // A CORS refusal reaches a page as an opaque network error with
                // no detail, so the guess has to be in the message: it is by far
                // the likeliest cause and the one with a fix the developer owns.
                "could not reach the sign-in server — if this build is served from a new \
                 address, register it as a redirect URI for this game first"
                    .to_string()
            })?
            .dyn_into()
            .map_err(|_| "the sign-in server sent something that was not a reply".to_string())?;
        let status = resp.status();
        let text = wasm_bindgen_futures::JsFuture::from(
            resp.text().map_err(|_| "could not read the reply".to_string())?,
        )
        .await
        .map_err(|_| "could not read the reply".to_string())?
        .as_string()
        .unwrap_or_default();
        Ok((status, text))
    }

    pub async fn post_form(url: &str, body: &str) -> Result<(u16, String), String> {
        let init = web_sys::RequestInit::new();
        init.set_method("POST");
        // No credentials: a page authenticates with the token it holds, never
        // with the player's fopull.com session cookie (contract §6.3).
        init.set_mode(web_sys::RequestMode::Cors);
        init.set_body(&wasm_bindgen::JsValue::from_str(body));
        let req = web_sys::Request::new_with_str_and_init(url, &init)
            .map_err(|_| "could not build the request".to_string())?;
        req.headers()
            .set("Content-Type", "application/x-www-form-urlencoded")
            .map_err(|_| "could not set the content type".to_string())?;
        send(req).await
    }

    pub async fn get_bearer(url: &str, access_token: &str) -> Result<(u16, String), String> {
        let init = web_sys::RequestInit::new();
        init.set_method("GET");
        init.set_mode(web_sys::RequestMode::Cors);
        let req = web_sys::Request::new_with_str_and_init(url, &init)
            .map_err(|_| "could not build the request".to_string())?;
        req.headers()
            .set("Authorization", &format!("Bearer {access_token}"))
            .map_err(|_| "could not set the authorization header".to_string())?;
        send(req).await
    }

    /// Step 1, from the page: stash the handshake, then leave.
    pub fn start(client: &WebClient) -> Result<(), String> {
        let (url, hs) = client.begin();
        stash(&hs)?;
        navigate(&url)
    }

    /// Step 3, at boot: if this load is a return from sign-in, finish it.
    ///
    /// `Ok(None)` is the ordinary case — most page loads are not redirects — and
    /// is deliberately not an error, because every boot runs this.
    pub async fn complete(client: &WebClient) -> Result<Option<Tokens>, String> {
        let query = current_query();
        // Peek without consuming: an ordinary page load must not throw away a
        // handshake belonging to a sign-in still in flight in another tab.
        let is_redirect = matches!(
            super::read_redirect(&query, None),
            Err(RedirectError::NoHandshake) | Ok(_)
        );
        if !is_redirect {
            return Ok(None);
        }
        let hs = take_stash();
        let code = match super::read_redirect(&query, hs.as_ref()) {
            Ok(c) => c,
            Err(RedirectError::NotARedirect) => return Ok(None),
            Err(e) => {
                scrub_query();
                return Err(e.message());
            }
        };
        // The code is about to be spent, and it is single-use — take it out of
        // the address bar before the await, not after, so a reload mid-exchange
        // cannot try to spend it again.
        scrub_query();
        let hs = hs.ok_or_else(|| RedirectError::NoHandshake.message())?;
        let (status, body) = post_form(&client.token_url(), &client.exchange_form(&code, &hs)).await?;
        super::parse_token_response(status, &body).map(Some)
    }

    /// Turn tokens into the [`Session`] the rest of the engine already speaks,
    /// by asking who this is and what they are entitled to.
    ///
    /// Both calls are cross-origin fetches and therefore CORS; a failure here
    /// leaves the player NOT signed in rather than half signed in, because a
    /// session with no identity is one the Inspector and the Hub would both
    /// render as blank.
    pub async fn identify(client: &WebClient, tokens: Tokens) -> Result<Session, String> {
        let (s, body) = get_bearer(&client.userinfo_url(), &tokens.access_token).await?;
        if !(200..300).contains(&s) {
            return Err(format!("the sign-in server would not say who this is ({s})"));
        }
        let who: crate::auth::UserInfo = serde_json::from_str(&body)
            .map_err(|e| format!("could not read the account: {e}"))?;
        // Entitlements are allowed to fail soft: not knowing the tier is a
        // signed-in player on the free tier, not a failed sign-in.
        let ent = match get_bearer(&client.entitlements_url(), &tokens.access_token).await {
            Ok((s, b)) if (200..300).contains(&s) => serde_json::from_str(&b).unwrap_or_default(),
            _ => crate::auth::Entitlements::default(),
        };
        Ok(Session::from_parts(tokens, who, ent))
    }

    /// Trade the stored refresh token for a fresh session, keeping the identity
    /// already known.
    ///
    /// **The answer carries a NEW refresh token and the old one is dead the
    /// moment it is used** (§6.2, rotation with reuse detection) — so the caller
    /// must persist what comes back before making another call, or the next
    /// refresh presents a rotated token and revokes the whole session.
    pub async fn refresh(client: &WebClient, session: &Session) -> Result<Session, String> {
        let rt = session
            .refresh_token
            .as_deref()
            .ok_or("your sign-in expired — sign in again")?;
        let (status, body) = post_form(&client.token_url(), &client.refresh_form(rt)).await?;
        let tokens = super::parse_token_response(status, &body)?;
        let mut fresh = session.clone();
        fresh.access_token = tokens.access_token;
        // Only replace the refresh token if a new one came back; a provider that
        // returns none has not rotated, and blanking it would sign the player out
        // at the next expiry for no reason.
        if tokens.refresh_token.is_some() {
            fresh.refresh_token = tokens.refresh_token;
        }
        Ok(fresh)
    }
}
