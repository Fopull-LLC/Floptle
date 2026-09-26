//! `cloud.*`: what a game reads from Floptle Cloud as itself, with no player
//! signed in.
//!
//! ```lua
//! cloud.avatar(net.identity(peer).id, 64, function(tex, err) end)  -- err "no_picture" when unset
//! cloud.config(function(config, err) end)                           -- the developer's remote config
//! cloud.get("/games/" .. cloud.game() .. "/rank/laps:canyon", function(res) end)
//! ```
//!
//! `account.*` acts as the signed-in player. This is the other credential the
//! Cloud API takes: the project's game key, from `project.ron`. The key is not
//! a secret (it ships in every build), and it only ever reads, so this is GET
//! only and reaches `/games/…` and `/players/…` and nothing else. The engine
//! attaches the key; a script never handles it.
//!
//! Requests go through `http.get`'s own path, so they are Play only and share
//! its rate limits and its cancellation on Stop.

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Function, Lua, Table, Value};

use crate::http_api;
use crate::texture_api::Fetcher;

/// The game a project is connected to, as the driver sets it.
#[derive(Clone, Debug, Default)]
pub(crate) struct CloudGame {
    /// The registered slug, `freeflier`.
    pub(crate) game: String,
    /// `fk_live_…`.
    pub(crate) key: String,
    /// `https://fopull.com`, or a dev instance from `FLOPTLE_ACCOUNT_BASE`.
    pub(crate) base: String,
}

pub(crate) type CloudState = Rc<RefCell<Option<CloudGame>>>;

const NOT_CONNECTED: &str = "this project is not connected to Floptle Cloud, so it has no game key to \
     read with. Connect it in ⚙ Settings ▸ Cloud";

/// The one header the Cloud API reads a game key from.
pub(crate) const GAME_KEY_HEADER: &str = "X-Floptle-Game-Key";

/// `path` under the API, if it is one a game key may read.
fn url_for(game: &CloudGame, path: &str) -> Result<String, String> {
    if path.contains("..") || path.contains("//") || path.contains("://") {
        return Err(format!("'{path}' is not a path on fopull.com"));
    }
    let head = path.trim_start_matches('/').split(['/', '?', '#']).next().unwrap_or("");
    if !path.starts_with('/') || !matches!(head, "games" | "players") {
        return Err(format!(
            "cloud.get reads /games/… and /players/… with the game key (got '{path}'); \
             what a player does goes through account.*"
        ));
    }
    Ok(format!("{}{}{path}", game.base.trim_end_matches('/'), floptle_account::cloud::API_PREFIX))
}

/// A path segment made safe for a URL: what a `sub` may contain is the
/// server's business, and a `/` or `?` in one must not become a different path.
fn segment(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub(crate) fn install(lua: &Lua, cloud: CloudState, fetcher: Fetcher) -> mlua::Result<()> {
    let t = lua.create_table()?;

    let c = cloud.clone();
    t.set("game", lua.create_function(move |_, ()| Ok(c.borrow().as_ref().map(|g| g.game.clone())))?)?;

    let (c, f) = (cloud.clone(), fetcher.clone());
    t.set(
        "get",
        lua.create_function(move |_, (path, cb): (String, Function)| {
            let game = c.borrow().clone().ok_or_else(|| mlua::Error::runtime(NOT_CONNECTED))?;
            let url = url_for(&game, &path).map_err(mlua::Error::runtime)?;
            let headers = vec![(GAME_KEY_HEADER.to_string(), game.key.clone())];
            http_api::send(&f.http, &f.logs, &f.in_fixed, "GET", url, None, headers, http_api::DEFAULT_TIMEOUT, true, cb)
        })?,
    )?;

    let (c, f) = (cloud.clone(), fetcher.clone());
    t.set(
        "config",
        lua.create_function(move |lua, cb: Function| {
            let game = c.borrow().clone().ok_or_else(|| mlua::Error::runtime(NOT_CONNECTED))?;
            let url = url_for(&game, &format!("/games/{}/config", segment(&game.game))).map_err(mlua::Error::runtime)?;
            let headers = vec![(GAME_KEY_HEADER.to_string(), game.key.clone())];
            // Hand the script the config itself, not the envelope around it.
            let unwrap = lua.create_function(move |lua, res: Table| {
                let config = res
                    .get::<Option<Table>>("json")?
                    .and_then(|j| j.get::<Option<Table>>("config").ok().flatten());
                match (res.get::<bool>("ok")?, config) {
                    (true, Some(cfg)) => cb.call::<()>((cfg, Value::Nil)),
                    (true, None) => cb.call::<()>((lua.create_table()?, Value::Nil)),
                    (false, _) => {
                        let why = res
                            .get::<Option<String>>("error")?
                            .unwrap_or_else(|| format!("the server answered HTTP {}", res.get::<u16>("status").unwrap_or(0)));
                        cb.call::<()>((Value::Nil, why))
                    }
                }
            })?;
            http_api::send(&f.http, &f.logs, &f.in_fixed, "GET", url, None, headers, http_api::DEFAULT_TIMEOUT, true, unwrap)
        })?,
    )?;

    let (c, f) = (cloud, fetcher);
    t.set(
        "avatar",
        lua.create_function(move |lua, args: mlua::MultiValue| {
            let mut it = args.into_iter();
            let sub = match it.next() {
                Some(Value::String(s)) if !s.as_bytes().is_empty() => s.to_str()?.to_string(),
                _ => return Err(mlua::Error::runtime("cloud.avatar takes a player id (net.identity(peer).id)")),
            };
            let (size, cb) = match (it.next(), it.next()) {
                (Some(Value::Function(cb)), None) => (128, cb),
                (Some(v), Some(Value::Function(cb))) => {
                    let n = match v {
                        Value::Integer(n) => n as i64,
                        Value::Number(n) => n as i64,
                        _ => -1,
                    };
                    if !matches!(n, 64 | 128 | 256) {
                        return Err(mlua::Error::runtime("cloud.avatar: size is 64, 128 or 256"));
                    }
                    (n, cb)
                }
                _ => return Err(mlua::Error::runtime("the last argument is the callback: function(tex, err) ... end")),
            };
            let game = c.borrow().clone().ok_or_else(|| mlua::Error::runtime(NOT_CONNECTED))?;
            let url = url_for(&game, &format!("/players/{}/avatar?size={size}", segment(&sub))).map_err(mlua::Error::runtime)?;
            let headers = vec![(GAME_KEY_HEADER.to_string(), game.key.clone())];
            f.fetch(lua, url, headers, http_api::DEFAULT_TIMEOUT, cb)
        })?,
    )?;

    lua.globals().set("cloud", t)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::http_api::HttpState;
    use crate::http_policy::HttpPolicy;
    use crate::texture_api::TextureLoads;
    use crate::ScriptLog;
    use std::cell::Cell;
    use std::io::{Read as _, Write as _};
    use std::sync::{Arc, Mutex};

    const KEY: &str = "fk_live_TEST";

    /// A loopback "fopull.com" that answers every request with one reply and
    /// keeps each request's head (lower-cased) for the test to read.
    fn fopull(status: &'static str, content_type: &'static str, body: Vec<u8>) -> (String, Arc<Mutex<Vec<String>>>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        std::thread::spawn(move || {
            for mut c in l.incoming().flatten() {
                let mut buf = [0u8; 4096];
                let n = c.read(&mut buf).unwrap_or(0);
                s.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase());
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = c.write_all(head.as_bytes());
                let _ = c.write_all(&body);
            }
        });
        (base, seen)
    }

    struct Harness {
        lua: Lua,
        http: Rc<RefCell<HttpState>>,
        book: Rc<RefCell<TextureLoads>>,
        cloud: CloudState,
        logs: Rc<RefCell<Vec<ScriptLog>>>,
    }

    fn harness(base: Option<&str>) -> Harness {
        let lua = Lua::new();
        let http = Rc::new(RefCell::new(HttpState::new()));
        http.borrow_mut().set_policy(HttpPolicy { allow_local: true });
        http.borrow_mut().set_playing(true);
        let logs: Rc<RefCell<Vec<ScriptLog>>> = Rc::default();
        let in_fixed = Rc::new(Cell::new(false));
        http_api::install_http_api(&lua, http.clone(), logs.clone(), in_fixed.clone());
        let book: Rc<RefCell<TextureLoads>> = Rc::default();
        let cloud: CloudState = Rc::default();
        if let Some(b) = base {
            *cloud.borrow_mut() = Some(CloudGame { game: "freeflier".into(), key: KEY.into(), base: b.into() });
        }
        let fetcher = Fetcher { book: book.clone(), http: http.clone(), logs: logs.clone(), in_fixed };
        install(&lua, cloud.clone(), fetcher).unwrap();
        Harness { lua, http, book, cloud, logs }
    }

    impl Harness {
        /// Frames until `done` says so.
        fn until(&self, done: impl Fn(&Self) -> bool) {
            for _ in 0..400 {
                http_api::drain(&self.lua, &self.http, &self.logs);
                crate::texture_api::drain(&self.lua, &self.book, &self.logs);
                if done(self) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            panic!("no answer");
        }
        fn global<T: mlua::FromLua>(&self, name: &str) -> T {
            self.lua.globals().get(name).unwrap()
        }
    }

    #[test]
    fn a_read_carries_the_game_key_and_reaches_only_the_game_surface() {
        let (base, seen) = fopull("200 OK", "application/json", br#"{"entries":[]}"#.to_vec());
        let h = harness(Some(&base));
        assert_eq!(h.lua.load("return cloud.game()").eval::<String>().unwrap(), "freeflier");
        h.lua.load("cloud.get('/games/freeflier/rank/laps', function(r) ok = r.ok; n = #r.json.entries end)").exec().unwrap();
        h.until(|h| h.global::<Option<bool>>("ok").is_some());
        assert!(h.global::<bool>("ok"));
        let req = seen.lock().unwrap()[0].clone();
        assert!(req.starts_with("get /api/floptle/v1/games/freeflier/rank/laps "), "{req}");
        assert!(req.contains("x-floptle-game-key: fk_live_test"), "no key sent: {req}");

        for path in ["/cloud/games/freeflier/key", "/wallet", "/games/../cloud", "https://evil.example/games/x"] {
            let e = h.lua.load(format!("cloud.get('{path}', function() end)")).exec().unwrap_err().to_string();
            assert!(e.contains("cloud.get reads") || e.contains("not a path"), "{path}: {e}");
        }
        assert_eq!(seen.lock().unwrap().len(), 1, "a refused path reached the server");

        *h.cloud.borrow_mut() = None;
        let e = h.lua.load("cloud.get('/games/x/config', function() end)").exec().unwrap_err().to_string();
        assert!(e.contains("not connected to Floptle Cloud"), "{e}");
    }

    #[test]
    fn an_avatar_arrives_as_texture_bytes_and_a_missing_one_says_no_picture() {
        let picture: Vec<u8> = (0..=255u8).collect();
        let (base, seen) = fopull("200 OK", "image/webp", picture.clone());
        let h = harness(Some(&base));
        // A sub with characters a URL would read as structure stays one segment.
        h.lua.load("cloud.avatar('user/1?x', 64, function(t, e) tex = t end)").exec().unwrap();
        let got: RefCell<Vec<crate::TextureRequest>> = RefCell::default();
        h.until(|h| {
            got.borrow_mut().extend(h.book.borrow_mut().take_requests());
            !got.borrow().is_empty()
        });
        assert_eq!(got.borrow()[0].bytes, picture, "the picture changed on the way");
        let req = seen.lock().unwrap()[0].clone();
        assert!(req.starts_with("get /api/floptle/v1/players/user%2f1%3fx/avatar?size=64 "), "{req}");
        assert!(req.contains("x-floptle-game-key: fk_live_test"), "{req}");

        let (base, _) = fopull("404 Not Found", "application/json", br#"{"error":"no_picture"}"#.to_vec());
        let h = harness(Some(&base));
        h.lua.load("cloud.avatar('nobody', function(t, e) err = e end)").exec().unwrap();
        h.until(|h| h.global::<Option<String>>("err").is_some());
        assert_eq!(h.global::<String>("err"), "no_picture");

        let e = h.lua.load("cloud.avatar('x', 100, function() end)").exec().unwrap_err().to_string();
        assert!(e.contains("64, 128 or 256"), "{e}");
    }

    #[test]
    fn config_hands_the_script_the_config_itself() {
        let (base, seen) = fopull("200 OK", "application/json", br#"{"config":{"motd":"hi","speed":2},"version":7}"#.to_vec());
        let h = harness(Some(&base));
        h.lua.load("cloud.config(function(c, e) motd = c.motd; speed = c.speed end)").exec().unwrap();
        h.until(|h| h.global::<Option<String>>("motd").is_some());
        assert_eq!(h.global::<String>("motd"), "hi");
        assert_eq!(h.global::<i64>("speed"), 2);
        assert!(seen.lock().unwrap()[0].starts_with("get /api/floptle/v1/games/freeflier/config "));
    }
}
