//! Authorized calls to the Floptle Cloud API.
//!
//! The whole point of this module is the sentence it refuses to let anyone
//! write: **the access token is never handed to the caller.** A script asks for
//! `/wallet`; this attaches the bearer, sends it to fopull.com and nowhere else,
//! and returns the reply. A shipped game's Lua is readable — anything a script
//! can hold, a player can read out of the file and post somewhere.
//!
//! So the host is fixed, the path is validated, and the token stays in Rust.

// Only the native transport measures a timeout; a browser build has no
// transport to bound.
#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;

/// Production. There is no other one — `dev-auth.fopull.com` is retired.
pub const DEFAULT_BASE: &str = "https://fopull.com";

/// Where the game-data API lives under the base. Identity endpoints (`/oauth/*`,
/// `/userinfo`) sit at the domain ROOT instead, pinned there by the contract, so
/// the two are not interchangeable and this prefix is applied only here.
pub const API_PREFIX: &str = "/api/floptle/v1";

// Native transport only — the browser build has no ureq call to parse for.
#[cfg(not(target_arch = "wasm32"))]
/// Largest reply accepted. The biggest documented Cloud payload is a 256 KB
/// save, so this is four times the largest legitimate answer — big enough to
/// never be the reason something fails, small enough that a confused endpoint
/// cannot hand a game an unbounded allocation.
const MAX_BODY: usize = 1024 * 1024;

/// One answer from the Cloud API. Deliberately the same shape the engine's
/// `http.*` layer produces, so a script sees one `res` table whichever it used.
pub struct CloudReply {
    pub status: u16,
    pub body: String,
    /// A transport-level failure (DNS, TLS, timeout). A 4xx is **not** an error:
    /// it is the server explaining itself, and the body says how.
    pub error: Option<String>,
    pub said_json: bool,
}

impl CloudReply {
    pub fn failed(msg: impl Into<String>) -> Self {
        Self { status: 0, body: String::new(), error: Some(msg.into()), said_json: false }
    }
}

/// Reject a path that could send the token somewhere else, or somewhere it has
/// no business being. Returns the full URL.
///
/// The interesting case is not `https://evil.com` — it is `//evil.com/x`, which
/// is a protocol-relative URL that a naive `format!("{base}{path}")` turns into
/// `https://fopull.com//evil.com/x` (harmless) but a URL parser somewhere down
/// the line may not. Refusing the shape is cheaper than reasoning about every
/// parser it will meet.
pub fn resolve(base: &str, path: &str) -> Result<String, String> {
    if !crate::auth::is_fopull_host(base) && !crate::auth::is_local_host(base) {
        return Err(format!(
            "the account base URL is {base}, which is not fopull.com — refusing to send an \
             access token there"
        ));
    }
    if !path.starts_with('/') {
        return Err(format!("a cloud path starts with '/' (got '{path}')"));
    }
    if path.starts_with("//") || path.contains("://") || path.contains("..") {
        return Err(format!("'{path}' is not a path on this server"));
    }
    let base = base.trim_end_matches('/');
    // A bare path gets the game-data prefix; an explicit `/oauth/...` or
    // `/userinfo` is left alone, because those are pinned to the domain root.
    let (full, rel) = if let Some(rest) = path.strip_prefix(API_PREFIX) {
        (format!("{base}{path}"), rest.to_string())
    } else if is_root_endpoint(path) {
        (format!("{base}{path}"), path.to_string())
    } else {
        (format!("{base}{API_PREFIX}{path}"), path.to_string())
    };
    script_may_call(&rel)?;
    Ok(full)
}

/// The paths a game's script may reach under the API, by first segment.
///
/// **A script acts as the player, not as the developer.** The token behind
/// `account.*` is the one the Hub signed in with, and that token can also
/// rotate a game's keys, read its usage and list its builds — for whoever is
/// signed in on the machine the game is running on. Nothing under `/cloud/`,
/// `/oauth/` or `/userinfo` is a thing a game has any business asking on a
/// player's behalf, so those are refused here, at the call, with the rule
/// named. The player-facing surface — wallet, missions, a game's own events
/// and saves, the player's own profile — is what a script gets.
pub const SCRIPT_PREFIXES: &[&str] = &["wallet", "games", "me", "missions"];

/// Is `rel` (the path below the API prefix) one a script may call?
pub fn script_may_call(rel: &str) -> Result<(), String> {
    let head = rel.trim_start_matches('/').split(['/', '?', '#']).next().unwrap_or("");
    if SCRIPT_PREFIXES.contains(&head) {
        return Ok(());
    }
    Err(format!(
        "'{rel}' is not a path a game may call — a script acts as the player, and reaches \
         /wallet, /missions, /games/... and /me/... only"
    ))
}

/// The identity endpoints the contract pins to the domain root, so they don't
/// get the `/api/floptle/v1` prefix applied to them.
fn is_root_endpoint(path: &str) -> bool {
    let head = path.split(['?', '/']).nth(1).unwrap_or("");
    matches!(head, "oauth" | "userinfo" | "entitlements" | "activate" | ".well-known")
}

/// Send one authorized request, **blocking**. Callers run this on a worker
/// thread — [`crate::Account`] does, and nothing else should call it directly
/// from a frame.
///
/// Native only. A browser cannot make this call at all — see
/// [`crate::auth::OfflineProvider`] for the three reasons — and
/// [`crate::Account::request`] refuses before it reaches here.
#[cfg(not(target_arch = "wasm32"))]
pub fn request(
    base: &str,
    access_token: &str,
    method: &str,
    path: &str,
    body: Option<String>,
    timeout: Duration,
) -> CloudReply {
    let url = match resolve(base, path) {
        Ok(u) => u,
        Err(e) => return CloudReply::failed(e),
    };
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    let mut req = match method.to_ascii_uppercase().as_str() {
        "POST" => agent.post(&url),
        "PUT" => agent.put(&url),
        "PATCH" => agent.request("PATCH", &url),
        "DELETE" => agent.delete(&url),
        _ => agent.get(&url),
    };
    req = req.set("Authorization", &format!("Bearer {access_token}"));
    req = req.set("Accept", "application/json");
    if body.is_some() {
        req = req.set("Content-Type", "application/json");
    }
    let res = match body {
        Some(b) => req.send_string(&b),
        None => req.call(),
    };
    match res {
        // ureq calls a 4xx an error; the Cloud API's uniform
        // `{error, error_description}` envelope lives in exactly those bodies,
        // so throwing them away would throw away every explanation.
        Ok(r) | Err(ureq::Error::Status(_, r)) => {
            let status = r.status();
            let said_json =
                r.header("content-type").is_some_and(|c| c.to_ascii_lowercase().contains("json"));
            use std::io::Read as _;
            let mut buf = String::new();
            let read = r.into_reader().take(MAX_BODY as u64 + 1).read_to_string(&mut buf);
            let error = match read {
                Err(e) => Some(format!("could not read the reply: {e}")),
                Ok(_) if buf.len() > MAX_BODY => {
                    buf.clear();
                    Some(format!("the reply is larger than the {MAX_BODY} byte limit"))
                }
                Ok(_) => None,
            };
            CloudReply { status, body: buf, error, said_json }
        }
        Err(e) => CloudReply::failed(format!("could not reach fopull.com: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_path_gets_the_game_data_prefix() {
        assert_eq!(
            resolve(DEFAULT_BASE, "/wallet").unwrap(),
            "https://fopull.com/api/floptle/v1/wallet"
        );
        // Already prefixed: left alone rather than doubled.
        assert_eq!(
            resolve(DEFAULT_BASE, "/api/floptle/v1/wallet").unwrap(),
            "https://fopull.com/api/floptle/v1/wallet"
        );
        // Query strings ride along.
        assert_eq!(
            resolve(DEFAULT_BASE, "/missions?game=fofighter").unwrap(),
            "https://fopull.com/api/floptle/v1/missions?game=fofighter"
        );
    }

    /// **A script reaches the player's surface and nothing else.** The
    /// developer endpoints under `/cloud/` are refused at the call with the
    /// rule named — whichever spelling of the prefix is used — and so are the
    /// identity endpoints at the root. `/wallet` and the rest still resolve.
    #[test]
    fn a_script_cannot_reach_developer_or_identity_endpoints() {
        for bad in [
            "/cloud/games",
            "/cloud/games/fofighter/key",
            "/api/floptle/v1/cloud/games/fofighter/key",
            "/cloud",
            "/userinfo",
            "/entitlements",
            "/oauth/token",
            "/.well-known/jwks.json",
            "/admin/anything",
        ] {
            let e = resolve(DEFAULT_BASE, bad).unwrap_err();
            assert!(e.contains("not a path a game may call"), "{bad}: {e}");
        }
        for ok in ["/wallet", "/missions?game=x", "/games/x/events", "/me/profile", "/api/floptle/v1/games/x/saves/1"] {
            resolve(DEFAULT_BASE, ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        // The head is the whole first segment: `/gamesX` is not `/games/`.
        assert!(resolve(DEFAULT_BASE, "/gamesX/y").is_err());
        assert!(resolve(DEFAULT_BASE, "/cloudy").is_err());
    }

    #[test]
    fn a_path_that_could_move_the_token_is_refused() {
        for bad in ["wallet", "//evil.com/x", "https://evil.com/x", "/../../oauth/token"] {
            assert!(resolve(DEFAULT_BASE, bad).is_err(), "{bad} should be refused");
        }
        // …and so is a base that isn't fopull.com, whatever the path says.
        assert!(resolve("https://evil.com", "/wallet").is_err());
        // A local dev provider is still allowed, for the same reason the Hub
        // allows one.
        assert!(resolve("http://localhost:8000", "/wallet").is_ok());
    }
}
