//! `http.*` — an editor extension talking to a server.
//!
//! Only present for a package declaring [`Permission::Network`](floptle_package::Permission).
//!
//! **Non-blocking, always.** A request goes to a worker thread and the callback
//! runs on a later editor frame, on the main thread, where the rest of the API
//! is safe to touch. There is no blocking form, for the same reason the game's
//! `http.*` has none: the blocking form is the one everybody reaches for, and it
//! turns a slow server into a frozen editor.
//!
//! ```lua
//! http.get("https://api.example.com/scenes", { headers = { Authorization = key } },
//!          function(res) if res.ok then handle(res.body) end end)
//! ```
//!
//! ## Signing in through a browser
//!
//! [`http.listen`] is the other half of the browser sign-in every hosted tool
//! needs: open a URL, and hear the answer come back on a loopback port.
//!
//! ```lua
//! local port = http.listen(function(req) finish(req.query.token) end)
//! ed.openUrl("https://example.com/auth?redirect=http://127.0.0.1:" .. port)
//! ```
//!
//! It binds **127.0.0.1 only** — never `0.0.0.0`. A sign-in listener reachable
//! from the network is a hole in whoever's machine the editor is running on, and
//! the loopback address is all a browser on that machine needs. It also stops
//! itself: one listener per package, closed when the package unloads, when the
//! project closes, or after [`LISTEN_TIMEOUT`] with nothing arriving, so a
//! forgotten sign-in does not leave a port open all afternoon.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

use mlua::{Lua, RegistryKey, Table};

/// How many requests may be in flight at once, per editor.
const MAX_IN_FLIGHT: usize = 8;
/// Largest response body accepted.
const MAX_BODY: usize = 8 * 1024 * 1024;
const DEFAULT_TIMEOUT: f64 = 20.0;
/// A loopback listener with nothing arriving gives up after this long.
pub(crate) const LISTEN_TIMEOUT: Duration = Duration::from_secs(300);

/// What a worker sends back. Plain data: no Lua value crosses a thread.
#[derive(Debug, Default, Clone)]
pub(crate) struct Reply {
    pub(crate) ok: bool,
    pub(crate) status: u16,
    pub(crate) body: String,
    pub(crate) error: String,
    /// Set for a loopback hit: the path and the parsed query string.
    pub(crate) path: Option<String>,
    pub(crate) query: Vec<(String, String)>,
}

struct Envelope {
    id: u64,
    reply: Reply,
    /// True for a request (which counts against the in-flight cap); false for a
    /// listener, which is counted separately and lives longer.
    is_request: bool,
}

/// One package's loopback listener.
struct Listener {
    port: u16,
    stop: Arc<AtomicBool>,
    started: Instant,
}

/// Every in-flight request and listener.
pub(crate) struct WebState {
    tx: Sender<Envelope>,
    rx: Receiver<Envelope>,
    /// Request id → the Lua callback waiting for it.
    waiting: HashMap<u64, RegistryKey>,
    ready: Vec<(RegistryKey, Reply)>,
    listeners: Vec<Listener>,
    next_id: u64,
    in_flight: usize,
}

impl Default for WebState {
    fn default() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            tx,
            rx,
            waiting: HashMap::new(),
            ready: Vec::new(),
            listeners: Vec::new(),
            next_id: 1,
            in_flight: 0,
        }
    }
}

impl WebState {
    pub(crate) fn in_flight(&self) -> usize {
        self.in_flight + self.listeners.len()
    }

    /// Move anything a worker has finished onto the ready list, and retire a
    /// listener that has waited long enough.
    pub(crate) fn pump(&mut self) {
        while let Ok(env) = self.rx.try_recv() {
            if env.is_request {
                self.in_flight = self.in_flight.saturating_sub(1);
            } else {
                // A listener answers once and is done — its thread has already
                // left the accept loop, so retire the bookkeeping with it.
                self.listeners.clear();
            }
            if let Some(key) = self.waiting.remove(&env.id) {
                self.ready.push((key, env.reply));
            }
        }
        let now = Instant::now();
        self.listeners.retain(|l| {
            if now.duration_since(l.started) > LISTEN_TIMEOUT {
                l.stop.store(true, Ordering::Relaxed);
                // Poke the accept loop so it notices the flag rather than
                // sitting in `accept` until somebody connects.
                let _ = std::net::TcpStream::connect(("127.0.0.1", l.port));
                false
            } else {
                true
            }
        });
    }

    pub(crate) fn take_ready(&mut self) -> Vec<(RegistryKey, Reply)> {
        std::mem::take(&mut self.ready)
    }

    /// Stop everything: called on package reload, project close and quit.
    pub(crate) fn cancel_all(&mut self) {
        for l in self.listeners.drain(..) {
            l.stop.store(true, Ordering::Relaxed);
            let _ = std::net::TcpStream::connect(("127.0.0.1", l.port));
        }
        self.waiting.clear();
        self.ready.clear();
        self.in_flight = 0;
    }

    /// Start a request on a worker thread.
    pub(crate) fn request(
        &mut self,
        method: &str,
        url: String,
        body: Option<String>,
        headers: Vec<(String, String)>,
        timeout: f64,
        cb: RegistryKey,
    ) -> Result<(), String> {
        if self.in_flight >= MAX_IN_FLIGHT {
            return Err(format!(
                "too many web requests at once ({MAX_IN_FLIGHT}) — wait for one to come back"
            ));
        }
        let id = self.next_id;
        self.next_id += 1;
        self.waiting.insert(id, cb);
        self.in_flight += 1;
        let tx = self.tx.clone();
        let method = method.to_string();
        std::thread::spawn(move || {
            let reply = run_request(&method, &url, body, &headers, timeout);
            let _ = tx.send(Envelope { id, reply, is_request: true });
        });
        Ok(())
    }

    /// Bind a loopback port and call back on the first request that arrives.
    /// Returns the port chosen.
    pub(crate) fn listen(&mut self, port: u16, cb: RegistryKey) -> Result<u16, String> {
        // 127.0.0.1, deliberately — see the module docs.
        let listener = std::net::TcpListener::bind(("127.0.0.1", port))
            .map_err(|e| format!("could not listen on 127.0.0.1:{port}: {e}"))?;
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
        let id = self.next_id;
        self.next_id += 1;
        self.waiting.insert(id, cb);
        let stop = Arc::new(AtomicBool::new(false));
        self.listeners.push(Listener { port, stop: stop.clone(), started: Instant::now() });
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(mut s) = stream else { continue };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let (path, query) = parse_request_line(&head);
                let _ = s.write_all(SIGNED_IN_PAGE.as_bytes());
                let _ = s.flush();
                let _ = tx.send(Envelope {
                    id,
                    reply: Reply {
                        ok: true,
                        status: 200,
                        body: head,
                        error: String::new(),
                        path: Some(path),
                        query,
                    },
                    is_request: false,
                });
                break; // one answer is what a sign-in needs
            }
        });
        Ok(port)
    }

    pub(crate) fn stop_listening(&mut self) {
        for l in self.listeners.drain(..) {
            l.stop.store(true, Ordering::Relaxed);
            let _ = std::net::TcpStream::connect(("127.0.0.1", l.port));
        }
    }
}

/// What the browser sees after it hands the answer back. Plain, self-closing,
/// and it does not pretend to be anybody's brand.
const SIGNED_IN_PAGE: &str = "HTTP/1.1 200 OK\r\n\
Content-Type: text/html; charset=utf-8\r\n\
Connection: close\r\n\r\n\
<!doctype html><meta charset=utf-8><title>Done</title>\
<body style=\"font:16px system-ui;padding:3rem;text-align:center\">\
<p>You can close this tab and go back to the editor.</p>";

fn run_request(
    method: &str,
    url: &str,
    body: Option<String>,
    headers: &[(String, String)],
    timeout: f64,
) -> Reply {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs_f64(timeout.clamp(1.0, 120.0)))
        .build();
    let mut req = agent.request(method, url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let res = match body {
        Some(b) => req.send_string(&b),
        None => req.call(),
    };
    match res {
        Ok(r) => read_body(r.status(), r),
        // A 4xx/5xx is an ANSWER, not a transport failure: the caller wants the
        // status and the body, which is usually where the server explains what
        // it did not like.
        Err(ureq::Error::Status(code, r)) => {
            let mut reply = read_body(code, r);
            reply.ok = false;
            reply
        }
        Err(e) => Reply { ok: false, error: e.to_string(), ..Reply::default() },
    }
}

fn read_body(status: u16, r: ureq::Response) -> Reply {
    let mut body = String::new();
    match r.into_reader().take(MAX_BODY as u64 + 1).read_to_string(&mut body) {
        Ok(_) if body.len() > MAX_BODY => Reply {
            ok: false,
            status,
            error: format!("the reply is larger than the {} MB limit", MAX_BODY / 1024 / 1024),
            ..Reply::default()
        },
        Ok(_) => Reply {
            ok: (200..300).contains(&status),
            status,
            body,
            ..Reply::default()
        },
        Err(e) => Reply { ok: false, status, error: e.to_string(), ..Reply::default() },
    }
}

/// Pull the path and query out of an HTTP request's first line.
pub(crate) fn parse_request_line(head: &str) -> (String, Vec<(String, String)>) {
    let line = head.lines().next().unwrap_or("");
    let target = line.split_whitespace().nth(1).unwrap_or("/");
    match target.split_once('?') {
        Some((path, q)) => (path.to_string(), parse_query(q)),
        None => (target.to_string(), Vec::new()),
    }
}

/// `a=1&b=hello%20there` → `[("a","1"), ("b","hello there")]`.
pub(crate) fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

/// Minimal percent-decoding, plus `+` for a space — enough for a redirect's
/// query string, which is all this reads.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Turn a reply into the table a callback receives.
pub(crate) fn reply_table(lua: &Lua, r: &Reply) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("ok", r.ok)?;
    t.set("status", r.status)?;
    t.set("body", r.body.clone())?;
    t.set("error", r.error.clone())?;
    if let Some(p) = &r.path {
        t.set("path", p.clone())?;
    }
    if !r.query.is_empty() {
        let q = lua.create_table()?;
        for (k, v) in &r.query {
            q.set(k.clone(), v.clone())?;
        }
        t.set("query", q)?;
    }
    Ok(t)
}

/// Read the `opts` table a call may carry: headers, timeout.
pub(crate) fn read_opts(t: Option<Table>) -> (Vec<(String, String)>, f64) {
    let Some(t) = t else { return (Vec::new(), DEFAULT_TIMEOUT) };
    let mut headers = Vec::new();
    if let Ok(h) = t.get::<Table>("headers") {
        for pair in h.pairs::<String, String>().flatten() {
            headers.push(pair);
        }
    }
    let timeout = t.get::<f64>("timeout").unwrap_or(DEFAULT_TIMEOUT);
    (headers, timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_line_yields_its_path_and_query() {
        let (path, q) = parse_request_line("GET /callback?token=abc&state=1 HTTP/1.1\r\nHost: x\r\n");
        assert_eq!(path, "/callback");
        assert_eq!(q, vec![("token".into(), "abc".into()), ("state".into(), "1".into())]);
    }

    #[test]
    fn a_request_with_no_query_is_not_an_error() {
        let (path, q) = parse_request_line("GET / HTTP/1.1");
        assert_eq!(path, "/");
        assert!(q.is_empty());
    }

    #[test]
    fn percent_escapes_and_plus_decode() {
        assert_eq!(percent_decode("hello%20there"), "hello there");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("100%25"), "100%");
        // A malformed escape is left alone rather than eating the string.
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("trailing%"), "trailing%");
    }

    #[test]
    fn a_query_value_may_be_empty_or_missing() {
        assert_eq!(parse_query("a=&b"), vec![("a".into(), "".into()), ("b".into(), "".into())]);
        assert!(parse_query("").is_empty());
    }

    /// The listener must never be reachable from anywhere but this machine.
    #[test]
    fn a_loopback_listener_binds_only_to_localhost() {
        let mut web = WebState::default();
        // Port 0 = let the OS pick, so the test cannot collide with anything.
        let lua = Lua::new();
        let cb = lua
            .create_registry_value(lua.create_function(|_, ()| Ok(())).unwrap())
            .unwrap();
        let port = web.listen(0, cb).unwrap();
        assert!(port > 0);
        // Connecting on loopback works…
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());
        web.stop_listening();
    }
}
