//! Pictures a game gets while it runs: a player's profile picture, an image a
//! player shared.
//!
//! ```lua
//! assets.textureFromUrl(url [, opts], function(tex, err) end)  -- opts as http.get
//! assets.textureFromBytes(bytes, function(tex, err) end)       -- e.g. a blob's body
//! assets.release(tex)                                          -- give it back
//! ```
//!
//! `tex` is a name like `"img:3"`, usable anywhere a texture path is: `draw.quad`,
//! a UI image's `texture`, a material's `texture`.
//!
//! The bytes are untrusted and are only ever read as pixels: the engine decodes
//! PNG, JPEG and WebP by their own signatures, refuses everything else, and
//! refuses a picture past the size limit from its header. A URL is fetched
//! exactly as `http.get` would fetch it (Play only, the same address policy and
//! rate limits), and a URL already loaded this session answers from memory.
//!
//! The script host keeps the book; the editor decodes on a worker, uploads on
//! the main thread, and answers. Callbacks run in the frame pass, like every
//! other reply a script waits for, and never inside the call that asked.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use mlua::{Function, Lua, Table, Value};

use crate::http_api::{self, HttpState};
use crate::{LogLevel, ScriptLog};

/// Bytes the driver is asked to turn into the texture `name`.
#[derive(Debug)]
pub struct TextureRequest {
    pub id: u64,
    pub name: String,
    pub bytes: Vec<u8>,
}

enum ByUrl {
    Loading(u64),
    Ready(String),
}

/// What scripts asked for, who is waiting, and what exists.
pub(crate) struct TextureLoads {
    /// Never reset, so a name is never reused within the process: a script
    /// holding an old name draws nothing rather than someone else's picture.
    next: u64,
    requests: Vec<TextureRequest>,
    waiting: HashMap<u64, Vec<Function>>,
    answers: Vec<(u64, Result<String, String>)>,
    by_url: HashMap<String, ByUrl>,
    url_of: HashMap<u64, String>,
    /// Textures that exist, by name.
    live: HashSet<String>,
    releases: Vec<String>,
    /// Whether this host draws at all. One that does not (a dedicated server,
    /// `floptle run`) answers at once, without downloading anything.
    can_draw: bool,
}

impl Default for TextureLoads {
    fn default() -> Self {
        Self {
            next: 1,
            requests: Vec::new(),
            waiting: HashMap::new(),
            answers: Vec::new(),
            by_url: HashMap::new(),
            url_of: HashMap::new(),
            live: HashSet::new(),
            releases: Vec::new(),
            can_draw: true,
        }
    }
}

const NO_GPU: &str = "this host draws nothing (a dedicated server, or floptle run), so it makes no textures";

impl TextureLoads {
    fn start(&mut self, cb: Function) -> u64 {
        let id = self.next;
        self.next += 1;
        self.waiting.insert(id, vec![cb]);
        id
    }

    fn request(&mut self, id: u64, bytes: Vec<u8>) {
        self.requests.push(TextureRequest { id, name: format!("img:{id}"), bytes });
    }

    pub(crate) fn take_requests(&mut self) -> Vec<TextureRequest> {
        std::mem::take(&mut self.requests)
    }

    pub(crate) fn take_releases(&mut self) -> Vec<String> {
        std::mem::take(&mut self.releases)
    }

    pub(crate) fn set_can_draw(&mut self, can: bool) {
        self.can_draw = can;
    }

    /// The driver's answer for a request: the texture now exists, or why not.
    pub(crate) fn answer(&mut self, id: u64, result: Result<String, String>) {
        let url = self.url_of.remove(&id);
        match &result {
            Ok(name) => {
                self.live.insert(name.clone());
                if let Some(u) = url {
                    self.by_url.insert(u, ByUrl::Ready(name.clone()));
                }
            }
            Err(_) => {
                if let Some(u) = url {
                    self.by_url.remove(&u);
                }
            }
        }
        self.answers.push((id, result));
    }

    /// `assets.release`: `true` if `name` was a texture this API made.
    fn release(&mut self, name: &str) -> bool {
        if !self.live.remove(name) {
            return false;
        }
        self.by_url.retain(|_, v| !matches!(v, ByUrl::Ready(n) if n == name));
        self.releases.push(name.to_string());
        true
    }

    /// Scene load: the callbacks close over nodes that are gone. A URL keeps a
    /// texture it already has for the next scene. A download still in flight
    /// is dropped with the rest of `http.*`'s, so that URL is forgotten and
    /// the next ask fetches it again, rather than waiting on a reply that is
    /// not coming. Bytes already handed to the driver still land, and with
    /// nobody to tell, are let go in [`drain`].
    pub(crate) fn cancel_all(&mut self) {
        self.waiting.clear();
        self.by_url.retain(|_, v| matches!(v, ByUrl::Ready(_)));
        self.url_of.clear();
    }

    /// Stop: the driver lets go of every texture, so the book starts over.
    pub(crate) fn reset(&mut self) {
        let next = self.next;
        *self = Self { next, can_draw: self.can_draw, ..Self::default() };
    }
}

pub(crate) fn install(
    lua: &Lua,
    state: Rc<RefCell<TextureLoads>>,
    http: Rc<RefCell<HttpState>>,
    logs: Rc<RefCell<Vec<ScriptLog>>>,
    in_fixed: Rc<Cell<bool>>,
) -> mlua::Result<()> {
    let assets: Table = lua.globals().get("assets")?;

    let st = state.clone();
    assets.set(
        "textureFromBytes",
        lua.create_function(move |_, (bytes, cb): (mlua::String, Function)| {
            let mut s = st.borrow_mut();
            let id = s.start(cb);
            if !s.can_draw {
                s.answer(id, Err(NO_GPU.into()));
            } else if bytes.as_bytes().len() > http_api::MAX_BODY {
                s.answer(id, Err(format!("larger than the {} byte limit", http_api::MAX_BODY)));
            } else {
                s.request(id, bytes.as_bytes().to_vec());
            }
            Ok(())
        })?,
    )?;

    let st = state.clone();
    assets.set(
        "textureFromUrl",
        lua.create_function(move |lua, args: mlua::MultiValue| {
            let (url, _, opts, cb) = http_api::parse_args(args.into_iter().collect(), false)?;
            let (headers, timeout, _) = http_api::read_opts("assets.textureFromUrl", opts.as_ref())?;
            let id = {
                let mut s = st.borrow_mut();
                match s.by_url.get(&url) {
                    Some(ByUrl::Ready(name)) => {
                        let name = name.clone();
                        let id = s.start(cb);
                        s.answers.push((id, Ok(name)));
                        return Ok(());
                    }
                    Some(ByUrl::Loading(id)) => {
                        let id = *id;
                        s.waiting.entry(id).or_default().push(cb);
                        return Ok(());
                    }
                    None => {}
                }
                let id = s.start(cb);
                if !s.can_draw {
                    s.answer(id, Err(NO_GPU.into()));
                    return Ok(());
                }
                s.by_url.insert(url.clone(), ByUrl::Loading(id));
                s.url_of.insert(id, url.clone());
                id
            };
            // The reply comes back through http.get's own machinery, into this
            // function, which hands the bytes on rather than to a script.
            let book = st.clone();
            let on_reply = lua.create_function(move |_, res: Table| {
                let mut s = book.borrow_mut();
                if res.get::<bool>("ok").unwrap_or(false) {
                    let body: mlua::String = res.get("body")?;
                    s.request(id, body.as_bytes().to_vec());
                } else {
                    let why = match res.get::<Option<String>>("error").ok().flatten() {
                        Some(e) => e,
                        None => format!("the server answered HTTP {}", res.get::<u16>("status").unwrap_or(0)),
                    };
                    s.answer(id, Err(why));
                }
                Ok(())
            })?;
            let sent = http_api::send(&http, &logs, &in_fixed, "GET", url.clone(), None, headers, timeout, false, on_reply);
            if let Err(e) = sent {
                // Refused at the call, as http.get would be: nothing is waiting.
                let mut s = st.borrow_mut();
                s.waiting.remove(&id);
                s.url_of.remove(&id);
                s.by_url.remove(&url);
                return Err(e);
            }
            Ok(())
        })?,
    )?;

    let st = state;
    assets.set("release", lua.create_function(move |_, name: String| Ok(st.borrow_mut().release(&name)))?)?;
    Ok(())
}

/// Call back every script whose texture has been answered: frame pass only.
pub(crate) fn drain(lua: &Lua, state: &Rc<RefCell<TextureLoads>>, logs: &Rc<RefCell<Vec<ScriptLog>>>) {
    // Collect with the borrow held, call with it released: a callback that
    // asks for another texture re-borrows the state.
    let ready: Vec<(Vec<Function>, Result<String, String>)> = {
        let Ok(mut s) = state.try_borrow_mut() else { return };
        let answers = std::mem::take(&mut s.answers);
        let mut out = Vec::new();
        for (id, result) in answers {
            match s.waiting.remove(&id) {
                Some(cbs) => out.push((cbs, result)),
                // Nobody left to tell (a scene load dropped the callback) and
                // no URL to find it by again: nothing could ever name it.
                None => {
                    if let Ok(name) = &result
                        && !s.by_url.values().any(|v| matches!(v, ByUrl::Ready(n) if n == name))
                    {
                        let name = name.clone();
                        s.release(&name);
                    }
                }
            }
        }
        out
    };
    for (cbs, result) in ready {
        for cb in cbs {
            let called = match &result {
                Ok(name) => cb.call::<()>((name.as_str(), Value::Nil)),
                Err(why) => match lua.create_string(why) {
                    Ok(w) => cb.call::<()>((Value::Nil, w)),
                    Err(e) => Err(e),
                },
            };
            if let Err(e) = called {
                logs.borrow_mut().push(ScriptLog {
                    level: LogLevel::Error,
                    msg: format!("texture callback: {e}"),
                    source: None,
                });
            }
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::http_policy::HttpPolicy;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Harness {
        lua: Lua,
        http: Rc<RefCell<HttpState>>,
        book: Rc<RefCell<TextureLoads>>,
        logs: Rc<RefCell<Vec<ScriptLog>>>,
    }

    fn harness() -> Harness {
        let lua = Lua::new();
        let http = Rc::new(RefCell::new(HttpState::new()));
        http.borrow_mut().set_policy(HttpPolicy { allow_local: true });
        http.borrow_mut().set_playing(true);
        let logs: Rc<RefCell<Vec<ScriptLog>>> = Rc::default();
        let fixed = Rc::new(Cell::new(false));
        http_api::install_http_api(&lua, http.clone(), logs.clone(), fixed.clone());
        lua.globals().set("assets", lua.create_table().unwrap()).unwrap();
        let book: Rc<RefCell<TextureLoads>> = Rc::default();
        install(&lua, book.clone(), http.clone(), logs.clone(), fixed).unwrap();
        lua.load("got = {}; function keep(tex, err) got[#got + 1] = { tex = tex, err = err } end").exec().unwrap();
        Harness { lua, http, book, logs }
    }

    impl Harness {
        fn run(&self, code: &str) {
            self.lua.load(code).exec().unwrap_or_else(|e| panic!("{code}: {e}"));
        }
        /// One frame pass: web replies, then texture answers.
        fn frame(&self) {
            http_api::drain(&self.lua, &self.http, &self.logs);
            drain(&self.lua, &self.book, &self.logs);
        }
        /// Frames until the driver has been handed something.
        fn requests(&self) -> Vec<TextureRequest> {
            for _ in 0..200 {
                self.frame();
                let r = self.book.borrow_mut().take_requests();
                if !r.is_empty() {
                    return r;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("nothing reached the driver");
        }
        /// `(tex, err)` for each callback so far.
        fn got(&self) -> Vec<(Option<String>, Option<String>)> {
            let t: Table = self.lua.globals().get("got").unwrap();
            t.sequence_values::<Table>()
                .map(|r| {
                    let r = r.unwrap();
                    (r.get("tex").unwrap(), r.get("err").unwrap())
                })
                .collect()
        }
    }

    /// Every byte value, NULs and all: what a picture looks like to a
    /// transport that assumed text.
    fn payload() -> Vec<u8> {
        (0..=255u8).chain(0..=255u8).collect()
    }

    fn serve(body: Vec<u8>) -> (String, Arc<AtomicUsize>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/avatar.png", l.local_addr().unwrap().port());
        let accepted = Arc::new(AtomicUsize::new(0));
        let n = accepted.clone();
        std::thread::spawn(move || {
            for mut c in l.incoming().flatten() {
                n.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 2048];
                let _ = c.read(&mut buf);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = c.write_all(head.as_bytes());
                let _ = c.write_all(&body);
            }
        });
        (url, accepted)
    }

    /// `http.get` hands a binary reply over whole. It used to read the reply
    /// as text and fail on the first byte that was not UTF-8.
    #[test]
    fn http_get_hands_over_a_binary_body_byte_for_byte() {
        let (url, _) = serve(payload());
        let h = harness();
        h.run(&format!("http.get('{url}', function(r) body = r.body; err = r.error end)"));
        for _ in 0..200 {
            h.frame();
            if h.lua.globals().get::<Option<mlua::String>>("body").unwrap().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let err: Option<String> = h.lua.globals().get("err").unwrap();
        assert_eq!(err, None);
        let body: mlua::String = h.lua.globals().get("body").unwrap();
        assert_eq!(body.as_bytes().to_vec(), payload());
    }

    #[test]
    fn a_picture_from_a_url_reaches_the_driver_whole_and_is_fetched_once() {
        let (url, accepted) = serve(payload());
        let h = harness();
        h.run(&format!("assets.textureFromUrl('{url}', keep)"));
        let reqs = h.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].bytes, payload(), "the bytes changed on the way");
        assert!(reqs[0].name.starts_with("img:"), "{}", reqs[0].name);
        assert!(h.got().is_empty(), "called back before the texture existed");

        h.book.borrow_mut().answer(reqs[0].id, Ok(reqs[0].name.clone()));
        h.frame();
        assert_eq!(h.got(), vec![(Some(reqs[0].name.clone()), None)]);

        // Asked again: answered from memory, with the same texture.
        h.run(&format!("assets.textureFromUrl('{url}', keep)"));
        h.frame();
        assert_eq!(h.got().len(), 2);
        assert_eq!(h.got()[1].0.as_deref(), Some(reqs[0].name.as_str()));
        assert_eq!(accepted.load(Ordering::SeqCst), 1, "fetched a URL it already had");
    }

    /// A dedicated server runs the same game script and has nothing to draw
    /// with. It says so, and never opens the socket.
    #[test]
    fn a_host_that_draws_nothing_answers_without_downloading() {
        let (url, accepted) = serve(payload());
        let h = harness();
        h.book.borrow_mut().set_can_draw(false);
        h.run(&format!("assets.textureFromUrl('{url}', keep); assets.textureFromBytes('xyz', keep)"));
        h.frame();
        let got = h.got();
        assert_eq!(got.len(), 2);
        assert!(got.iter().all(|(t, e)| t.is_none() && e.as_deref().is_some_and(|e| e.contains("draws nothing"))), "{got:?}");
        assert!(h.book.borrow_mut().take_requests().is_empty());
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(accepted.load(Ordering::SeqCst), 0, "downloaded with nothing to draw it on");
    }

    #[test]
    fn a_refusal_reaches_the_callback_and_only_a_made_texture_can_be_released() {
        let h = harness();
        h.run("assets.textureFromBytes('not a picture', keep); assets.textureFromBytes('\\0\\255', keep)");
        let reqs = h.book.borrow_mut().take_requests();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[1].bytes, vec![0, 255]);
        h.book.borrow_mut().answer(reqs[0].id, Err("not a PNG, JPEG or WebP image".into()));
        h.book.borrow_mut().answer(reqs[1].id, Ok(reqs[1].name.clone()));
        h.frame();
        assert_eq!(
            h.got(),
            vec![(None, Some("not a PNG, JPEG or WebP image".into())), (Some(reqs[1].name.clone()), None)]
        );

        let release = |n: &str| -> bool { h.lua.load(format!("return assets.release('{n}')")).eval().unwrap() };
        assert!(!release("textures/wall.png"), "released a project texture");
        assert!(!release(&reqs[0].name), "released a texture that was never made");
        assert!(release(&reqs[1].name));
        assert!(!release(&reqs[1].name), "released twice");
        assert_eq!(h.book.borrow_mut().take_releases(), vec![reqs[1].name.clone()]);
    }

    /// A scene load drops every download in flight. A URL caught in one is
    /// fetched again when the next scene asks, rather than waiting forever on
    /// the reply that was dropped.
    #[test]
    fn a_url_whose_download_a_scene_load_dropped_is_fetched_again() {
        let (url, accepted) = serve(payload());
        let h = harness();
        h.run(&format!("assets.textureFromUrl('{url}', keep)"));
        h.http.borrow_mut().cancel_all();
        h.book.borrow_mut().cancel_all();
        h.run(&format!("assets.textureFromUrl('{url}', keep)"));
        let reqs = h.requests();
        h.book.borrow_mut().answer(reqs[0].id, Ok(reqs[0].name.clone()));
        h.frame();
        assert_eq!(h.got(), vec![(Some(reqs[0].name.clone()), None)], "the second scene's ask was never answered");
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
    }
}
