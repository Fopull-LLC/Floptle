//! A game's Cloud collections, read and declared from the command line.
//!
//! Reading is the game's: `GET /games/{slug}/collections` answers the game key,
//! which the project already holds, so checking a setup needs nobody signed in.
//! Declaring is the owner's: `PUT /cloud/games/{slug}/collections/{name}` takes
//! the developer's own token, which stays in this crate like every other one.
//!
//! There is deliberately no delete here. Deleting a collection deletes
//! everything in it, and that stays a click on the game's page.

use std::time::Duration;

use crate::builds::{check_slug, error_of, exchange};
use crate::cloud::API_PREFIX;

/// The header the Cloud API reads a game key from.
const GAME_KEY_HEADER: &str = "X-Floptle-Game-Key";

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(Duration::from_secs(30)).build()
}

/// Every collection `game` has on the server, as the server describes it:
/// `name`, `kind`, `access`, `authority`, `cap_kb`, `keep` or `mode`, and
/// `declared` (false for one a write created before declaring was required).
pub fn read_declared(base: &str, game: &str, key: &str) -> Result<Vec<serde_json::Value>, String> {
    check_slug(game)?;
    let url = format!("{}{API_PREFIX}/games/{game}/collections", base.trim_end_matches('/'));
    let req = agent().get(&url).set(GAME_KEY_HEADER, key).set("Accept", "application/json");
    let (status, text) = exchange(req, None).map_err(|e| format!("could not reach fopull.com: {e}"))?;
    if !(200..300).contains(&status) {
        return Err(error_of(status, &text));
    }
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("fopull.com's collection list is not JSON: {e}"))?;
    v.get("collections")
        .and_then(|c| c.as_array())
        .cloned()
        .ok_or_else(|| "fopull.com's answer has no collection list".to_string())
}

/// Declare (or change) one collection with `token`. Answers the HTTP status on
/// success: 201 made, 200 changed. A refusal, `409 collection_not_empty`
/// included, comes back as the server's own sentence.
pub(crate) fn declare_with_token(
    base: &str,
    token: &str,
    game: &str,
    name: &str,
    declaration: &serde_json::Value,
) -> Result<u16, String> {
    check_slug(game)?;
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') {
        return Err(format!("'{name}' is not a collection name (a-z, 0-9, - and _)"));
    }
    let url = format!("{}{API_PREFIX}/cloud/games/{game}/collections/{name}", base.trim_end_matches('/'));
    let req = agent()
        .put(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/json")
        .set("Content-Type", "application/json");
    let (status, text) = exchange(req, Some(declaration.to_string().as_bytes()))
        .map_err(|e| format!("could not reach fopull.com: {e}"))?;
    if !(200..300).contains(&status) {
        return Err(error_of(status, &text));
    }
    Ok(status)
}

impl crate::Account {
    /// Declare one of `game`'s collections as the signed-in developer, who has
    /// to own the game. **Blocking.**
    pub fn declare_collection(&self, game: &str, name: &str, declaration: &serde_json::Value) -> Result<u16, String> {
        let token = self.access_token()?;
        declare_with_token(self.base(), &token, game, name, declaration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::sync::{Arc, Mutex};

    /// A loopback site that answers every request with one reply and keeps
    /// each request whole (head lower-cased, then the body) for the test.
    fn site(status: &'static str, body: &'static str) -> (String, Arc<Mutex<Vec<String>>>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        std::thread::spawn(move || {
            for mut c in l.incoming().flatten() {
                let mut got = Vec::new();
                let mut buf = [0u8; 4096];
                // Head, then as much body as it says it has.
                loop {
                    let n = c.read(&mut buf).unwrap_or(0);
                    got.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&got).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text[..end]
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                            .unwrap_or(0);
                        if got.len() >= end + 4 + len {
                            break;
                        }
                    }
                    if n == 0 {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&got).to_string();
                let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
                s.lock().unwrap().push(format!("{}\r\n\r\n{rest}", head.to_ascii_lowercase()));
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = c.write_all(reply.as_bytes());
            }
        });
        (base, seen)
    }

    #[test]
    fn the_list_is_read_with_the_game_key_and_no_account() {
        let (base, seen) = site("200 OK", r#"{"game":"freeflier","collections":[{"name":"time","kind":"rank","keep":"min","declared":true}]}"#);
        let got = read_declared(&base, "freeflier", "fk_live_X").unwrap();
        assert_eq!(got[0]["keep"], "min");
        let req = seen.lock().unwrap()[0].clone();
        assert!(req.starts_with("get /api/floptle/v1/games/freeflier/collections "), "{req}");
        assert!(req.contains("x-floptle-game-key: fk_live_x"), "{req}");
        assert!(!req.contains("authorization"), "a read sent an account token: {req}");
    }

    #[test]
    fn a_declaration_is_one_owner_put_and_a_refusal_keeps_the_servers_words() {
        let (base, seen) = site("201 Created", r#"{"name":"laps"}"#);
        let body = serde_json::json!({"kind":"rank","access":"public","authority":"player","keep":"min"});
        assert_eq!(declare_with_token(&base, "tok", "freeflier", "laps", &body), Ok(201));
        let req = seen.lock().unwrap()[0].clone();
        assert!(req.starts_with("put /api/floptle/v1/cloud/games/freeflier/collections/laps "), "{req}");
        assert!(req.contains("authorization: bearer tok"), "{req}");
        let sent: serde_json::Value = serde_json::from_str(req.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(sent, body);

        let (base, _) = site(
            "409 Conflict",
            r#"{"error":"collection_not_empty","error_description":"replays holds 12 objects; kind and access change only while it is empty."}"#,
        );
        let e = declare_with_token(&base, "tok", "freeflier", "replays", &body).unwrap_err();
        assert!(e.contains("409") && e.contains("holds 12 objects"), "{e}");

        let e = declare_with_token(&base, "tok", "freeflier", "../keys", &body).unwrap_err();
        assert!(e.contains("not a collection name"), "{e}");
    }
}
