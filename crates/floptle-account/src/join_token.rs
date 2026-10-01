//! Join tokens, from the account's side (`contracts/identity-auth.md` §8).
//!
//! A player joining a server asks fopull.com for a token that names that one
//! server, and hands the server the token. The access token is spent here,
//! at fopull.com, and never travels further. The server checks the token
//! against the provider's published keys, which [`fetch_jwks`] reads for it.
//!
//! Neither call is open to a game's script: a script never holds a token of
//! any kind, and a join token is the engine's business at join time.

use std::time::Duration;

/// Where a join token is minted. At the domain root, like every identity
/// endpoint.
pub const MINT_PATH: &str = "/oauth/join-token";

/// The provider's signing keys. Public, and fetched with no account at all.
pub const JWKS_PATH: &str = "/.well-known/jwks.json";

/// A token is small; anything this size is not one.
const MAX_REPLY: usize = 64 * 1024;

/// `base` + `path`, refused for any host that is not fopull.com or a local
/// development instance, so an access token can never be sent elsewhere.
fn url_for(base: &str, path: &str) -> Result<String, String> {
    if !crate::auth::is_fopull_host(base) && !crate::auth::is_local_host(base) {
        return Err(format!(
            "the account base URL is {base}, which is not fopull.com — refusing to send an \
             access token there"
        ));
    }
    Ok(format!("{}{path}", base.trim_end_matches('/')))
}

/// The token out of a mint reply, or why there is none in a sentence a
/// console can show.
pub fn read_mint_reply(status: u16, body: &[u8]) -> Result<String, String> {
    let json: Option<serde_json::Value> = serde_json::from_slice(body).ok();
    let field = |k: &str| json.as_ref().and_then(|j| j.get(k)).and_then(|v| v.as_str()).map(str::to_string);
    match status {
        200 => field("join_token").filter(|t| !t.is_empty()).ok_or_else(|| {
            "fopull.com answered the join-token request without a token".to_string()
        }),
        400 => Err(format!(
            "fopull.com cannot mint a token for this address ({})",
            field("error_description").or_else(|| field("error")).unwrap_or_else(|| "invalid_audience".into())
        )),
        401 => Err("your sign-in was not accepted for a join token — sign in again".into()),
        429 => Err("too many join tokens this minute — the join goes ahead unverified".into()),
        s => Err(format!("fopull.com answered the join-token request with HTTP {s}")),
    }
}

fn mint_body(audience: &str) -> Vec<u8> {
    serde_json::json!({ "audience": audience }).to_string().into_bytes()
}

/// Mint a join token for `audience`, **blocking**. Worker threads only.
#[cfg(not(target_arch = "wasm32"))]
pub fn mint(base: &str, access_token: &str, audience: &str, timeout: Duration) -> Result<String, String> {
    let url = url_for(base, MINT_PATH)?;
    let res = ureq::AgentBuilder::new()
        .timeout(timeout)
        .build()
        .post(&url)
        .set("Authorization", &format!("Bearer {access_token}"))
        .set("Accept", "application/json")
        .set("Content-Type", "application/json")
        .send_bytes(&mint_body(audience));
    match res {
        Ok(r) | Err(ureq::Error::Status(_, r)) => {
            let status = r.status();
            let mut body = Vec::new();
            use std::io::Read as _;
            r.into_reader()
                .take(MAX_REPLY as u64)
                .read_to_end(&mut body)
                .map_err(|e| format!("could not read the join-token reply: {e}"))?;
            read_mint_reply(status, &body)
        }
        Err(e) => Err(format!("could not reach fopull.com for a join token: {e}")),
    }
}

/// The same from a page, through `fetch`. fopull.com allows a registered
/// game's origin to make this call.
#[cfg(target_arch = "wasm32")]
pub async fn mint_web(base: &str, access_token: &str, audience: &str, timeout: Duration) -> Result<String, String> {
    let url = url_for(base, MINT_PATH)?;
    let headers = vec![
        ("Authorization".to_string(), format!("Bearer {access_token}")),
        ("Accept".to_string(), "application/json".to_string()),
        ("Content-Type".to_string(), "application/json".to_string()),
    ];
    let r = crate::web_fetch::fetch("POST", &url, &headers, Some(mint_body(audience)), timeout.as_secs_f64(), MAX_REPLY)
        .await?;
    read_mint_reply(r.status, &r.body)
}

/// The provider's key set, as text, for `floptle_net::JoinTokenCheck::set_keys`.
/// **Blocking**, and needs no account: the keys are public.
#[cfg(not(target_arch = "wasm32"))]
pub fn fetch_jwks(base: &str, timeout: Duration) -> Result<String, String> {
    let url = url_for(base, JWKS_PATH)?;
    let r = ureq::AgentBuilder::new()
        .timeout(timeout)
        .build()
        .get(&url)
        .call()
        .map_err(|e| format!("could not fetch fopull.com's signing keys: {e}"))?;
    let mut body = Vec::new();
    use std::io::Read as _;
    r.into_reader()
        .take(MAX_REPLY as u64)
        .read_to_end(&mut body)
        .map_err(|e| format!("could not read fopull.com's signing keys: {e}"))?;
    String::from_utf8(body).map_err(|_| "fopull.com's signing keys are not text".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mint_reply_yields_the_token_or_a_reason() {
        let ok = br#"{"join_token":"eyJ.a.b","expires_in":300,"audience":"cloud://UABCDE"}"#;
        assert_eq!(read_mint_reply(200, ok).as_deref(), Ok("eyJ.a.b"));
        assert!(read_mint_reply(200, b"{}").is_err(), "a 200 without a token is not a token");
        let bad = br#"{"error":"invalid_audience","error_description":"wss is not a join target"}"#;
        assert!(read_mint_reply(400, bad).unwrap_err().contains("wss is not a join target"));
        assert!(read_mint_reply(401, b"").unwrap_err().contains("sign in"));
        assert!(read_mint_reply(429, b"").unwrap_err().contains("unverified"));
        assert!(read_mint_reply(502, b"<html>").unwrap_err().contains("502"));
    }

    /// The access token rides on this request, so the host is checked before
    /// anything is sent.
    #[test]
    fn an_access_token_goes_to_fopull_com_and_nowhere_else() {
        assert_eq!(url_for("https://fopull.com/", MINT_PATH).as_deref(), Ok("https://fopull.com/oauth/join-token"));
        assert!(url_for("http://127.0.0.1:8080", MINT_PATH).is_ok(), "a local dev instance");
        for evil in ["https://fopull.com.evil.example", "https://evil.example/fopull.com", "https://notfopull.com"] {
            assert!(url_for(evil, MINT_PATH).is_err(), "{evil}");
        }
    }

    /// What goes over the wire: the bearer, and the audience as JSON. Against a
    /// loopback "fopull.com", read back raw.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_mint_sends_the_bearer_and_the_audience() {
        use std::io::{Read as _, Write as _};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
        let server = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            while !String::from_utf8_lossy(&got).contains("}") {
                let n = c.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
            }
            let body = br#"{"join_token":"tok","expires_in":300}"#;
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            c.write_all(head.as_bytes()).unwrap();
            c.write_all(body).unwrap();
            String::from_utf8_lossy(&got).to_string()
        });
        let t = mint(&base, "the-access-token", "cloud://UABCDE", Duration::from_secs(5));
        let sent = server.join().unwrap();
        assert_eq!(t.as_deref(), Ok("tok"));
        assert!(sent.starts_with("POST /oauth/join-token "), "{sent}");
        assert!(sent.to_ascii_lowercase().contains("authorization: bearer the-access-token"), "{sent}");
        assert!(sent.contains(r#"{"audience":"cloud://UABCDE"}"#), "{sent}");
    }
}
