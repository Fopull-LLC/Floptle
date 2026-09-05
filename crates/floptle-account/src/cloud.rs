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
    let full = if path.starts_with(API_PREFIX) || is_root_endpoint(path) {
        format!("{base}{path}")
    } else {
        format!("{base}{API_PREFIX}{path}")
    };
    Ok(full)
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

    #[test]
    fn the_identity_endpoints_stay_at_the_root() {
        // The contract pins these; prefixing them would 404 in a way that reads
        // like the account is broken rather than like the URL is wrong.
        for p in ["/userinfo", "/entitlements", "/oauth/token", "/.well-known/jwks.json"] {
            assert_eq!(
                resolve(DEFAULT_BASE, p).unwrap(),
                format!("https://fopull.com{p}"),
                "{p} should not be prefixed"
            );
        }
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
