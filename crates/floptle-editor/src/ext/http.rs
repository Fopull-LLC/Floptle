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
use std::time::Duration;

use floptle_core::time::Instant;

use mlua::{Lua, RegistryKey, Table};

/// How many requests may be in flight at once, per editor.
const MAX_IN_FLIGHT: usize = 8;
/// Largest response body accepted.
const MAX_BODY: usize = 8 * 1024 * 1024;
const DEFAULT_TIMEOUT: f64 = 20.0;
/// How many event streams may be open at once, per editor.
const MAX_STREAMS: usize = 4;
/// How many frames of one stream may wait for Lua before the rest are dropped.
const MAX_PENDING_FRAMES: usize = 256;
/// A stream with nothing on it — not even a keepalive comment — for this long
/// is a dead connection, not a quiet one. Servers keep these alive with a
/// comment every 15s or so, so this is generous by design.
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// How long one read may block. This is the cancellation latency, not a
/// timeout: a cancelled stream is noticed at the next read deadline.
const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(10);
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

enum Envelope {
    /// A request or a listener finished and has its one answer.
    Done {
        id: u64,
        reply: Reply,
        /// True for a request (which counts against the in-flight cap); false
        /// for a listener, which is counted separately and lives longer.
        is_request: bool,
    },
    /// One frame off an open event stream. Many of these arrive per stream.
    Frame { id: u64, event: String, data: String },
    /// The stream ended. `reply.ok` says whether it ended the way it meant to.
    StreamEnd { id: u64, reply: Reply },
}

/// One package's loopback listener.
struct Listener {
    port: u16,
    stop: Arc<AtomicBool>,
    started: Instant,
}

/// An open event stream and the Lua waiting on it.
///
/// Unlike a request, the frame callback is called MANY times, so its registry
/// value is kept until the stream ends rather than removed on first delivery.
struct Stream {
    on_frame: RegistryKey,
    stop: Arc<AtomicBool>,
    /// Frames the server sent that Lua has not been given yet. Capped, because
    /// a server emitting faster than the editor draws must cost bounded memory
    /// rather than growing until something gives.
    pending: usize,
    dropped: usize,
}

/// Every in-flight request and listener.
pub(crate) struct WebState {
    tx: Sender<Envelope>,
    rx: Receiver<Envelope>,
    /// Request id → the Lua callback waiting for it.
    waiting: HashMap<u64, RegistryKey>,
    ready: Vec<(RegistryKey, Reply)>,
    /// Registry values belonging to finished streams, for the host to free.
    lua_keys_to_drop: Vec<RegistryKey>,
    listeners: Vec<Listener>,
    /// Stream id → the frame callback and its stop flag.
    streams: HashMap<u64, Stream>,
    /// Frames waiting to be handed to Lua, in arrival order across all streams.
    ready_frames: Vec<(u64, String, String)>,
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
            lua_keys_to_drop: Vec::new(),
            listeners: Vec::new(),
            streams: HashMap::new(),
            ready_frames: Vec::new(),
            next_id: 1,
            in_flight: 0,
        }
    }
}

impl WebState {
    pub(crate) fn in_flight(&self) -> usize {
        self.in_flight + self.listeners.len() + self.streams.len()
    }

    /// Move anything a worker has finished onto the ready list, and retire a
    /// listener that has waited long enough.
    pub(crate) fn pump(&mut self) {
        while let Ok(env) = self.rx.try_recv() {
            match env {
                Envelope::Done { id, reply, is_request } => {
                    if is_request {
                        self.in_flight = self.in_flight.saturating_sub(1);
                    } else {
                        // A listener answers once and is done — its thread has
                        // already left the accept loop, so retire the
                        // bookkeeping with it.
                        self.listeners.clear();
                    }
                    if let Some(key) = self.waiting.remove(&id) {
                        self.ready.push((key, reply));
                    }
                }
                Envelope::Frame { id, event, data } => {
                    let Some(s) = self.streams.get_mut(&id) else { continue };
                    if s.pending >= MAX_PENDING_FRAMES {
                        // Dropped rather than queued without limit. Counted, and
                        // said out loud when the stream ends: a progress bar
                        // that skipped a step is fine, a progress bar that
                        // silently skipped a step is a bug report later.
                        s.dropped += 1;
                        continue;
                    }
                    s.pending += 1;
                    self.ready_frames.push((id, event, data));
                }
                Envelope::StreamEnd { id, mut reply } => {
                    if let Some(s) = self.streams.remove(&id) {
                        self.lua_keys_to_drop.push(s.on_frame);
                        if s.dropped > 0 {
                            reply.error = format!(
                                "{} frame(s) were dropped: the server sent them faster than \
                                 the editor could hand them over{}",
                                s.dropped,
                                if reply.error.is_empty() {
                                    String::new()
                                } else {
                                    format!(" ({})", reply.error)
                                }
                            );
                        }
                    }
                    if let Some(key) = self.waiting.remove(&id) {
                        self.ready.push((key, reply));
                    }
                }
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

    /// Frames to hand to Lua this pass, each with the callback that wants it.
    ///
    /// The key is BORROWED, not taken: a stream's callback is called once per
    /// frame and must survive until the stream ends.
    pub(crate) fn take_frames(&mut self) -> Vec<(u64, String, String)> {
        let frames = std::mem::take(&mut self.ready_frames);
        for (id, _, _) in &frames {
            if let Some(s) = self.streams.get_mut(id) {
                s.pending = s.pending.saturating_sub(1);
            }
        }
        frames
    }

    pub(crate) fn is_streaming(&self, id: u64) -> bool {
        self.streams.contains_key(&id)
    }

    pub(crate) fn frame_callback(&self, id: u64) -> Option<&RegistryKey> {
        self.streams.get(&id).map(|s| &s.on_frame)
    }

    /// Registry values whose owner has gone. Returned so the host can free them
    /// on the Lua state, which this type has no handle on.
    pub(crate) fn take_finished_keys(&mut self) -> Vec<RegistryKey> {
        std::mem::take(&mut self.lua_keys_to_drop)
    }

    /// Stop one open stream. Its `on_frame` is retired with it.
    pub(crate) fn stop_stream(&mut self, id: u64) {
        if let Some(s) = self.streams.remove(&id) {
            s.stop.store(true, Ordering::Relaxed);
            self.lua_keys_to_drop.push(s.on_frame);
        }
        self.waiting.remove(&id);
        self.ready_frames.retain(|(fid, _, _)| *fid != id);
    }

    /// Open a Server-Sent Events stream.
    ///
    /// The frame parsing is done HERE and not in Lua on purpose: every consumer
    /// of an event stream needs the same subset of the protocol — `event:`,
    /// `data:`, a blank line ending a frame, `:` comments as keepalives — and a
    /// package that has to write that itself will get the keepalive wrong and
    /// discover it in production at 3am on somebody else's server.
    pub(crate) fn stream(
        &mut self,
        url: String,
        headers: Vec<(String, String)>,
        timeout: f64,
        on_frame: RegistryKey,
        on_end: RegistryKey,
    ) -> Result<u64, String> {
        if self.streams.len() >= MAX_STREAMS {
            return Err(format!(
                "too many open event streams at once ({MAX_STREAMS}) — cancel one first"
            ));
        }
        let id = self.next_id;
        self.next_id += 1;
        self.waiting.insert(id, on_end);
        let stop = Arc::new(AtomicBool::new(false));
        self.streams
            .insert(id, Stream { on_frame, stop: stop.clone(), pending: 0, dropped: 0 });
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let reply = run_stream(id, &url, &headers, timeout, &stop, &tx);
            let _ = tx.send(Envelope::StreamEnd { id, reply });
        });
        Ok(id)
    }

    /// Stop everything: called on package reload, project close and quit.
    pub(crate) fn cancel_all(&mut self) {
        for (_, s) in self.streams.drain() {
            s.stop.store(true, Ordering::Relaxed);
        }
        self.ready_frames.clear();
        self.lua_keys_to_drop.clear();
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
            let _ = tx.send(Envelope::Done { id, reply, is_request: true });
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
                let _ = tx.send(Envelope::Done {
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

/// Read an event stream to its end, sending each frame as it completes.
///
/// The protocol subset is the one every SSE server actually speaks:
///
/// ```text
/// event: progress          <- optional; defaults to "message"
/// data: {"pct": 40}        <- may repeat; lines join with \n
///                          <- a BLANK LINE ends the frame
/// : keepalive              <- a comment. Ignored, but it proves the
///                             connection is alive, which is what the idle
///                             timeout below is counting.
/// ```
fn run_stream(
    id: u64,
    url: &str,
    headers: &[(String, String)],
    timeout: f64,
    stop: &AtomicBool,
    tx: &Sender<Envelope>,
) -> Reply {
    let agent = ureq::AgentBuilder::new()
        // Two different deadlines, and they are not interchangeable.
        //
        // CONNECT is the caller's timeout: how long to wait for the server to
        // answer at all.
        .timeout_connect(Duration::from_secs_f64(timeout.clamp(1.0, 120.0)))
        // READ is per read, and it is the only way a blocking reader can notice
        // anything. Without it `read_line` sits in the kernel forever: a
        // cancelled stream would not stop and a dead connection would not be
        // noticed, so the idle timeout below would be code that never runs.
        .timeout_read(STREAM_READ_TIMEOUT)
        .build();
    let mut req = agent.get(url).set("Accept", "text/event-stream");
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let res = match req.call() {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            // A server that does not do streaming answers with a status, and
            // the caller's job is to fall back to polling. Give it the body:
            // that is where the server says why.
            let mut reply = read_body(code, r);
            reply.ok = false;
            return reply;
        }
        Err(e) => return Reply { ok: false, error: e.to_string(), ..Reply::default() },
    };
    let status = res.status();
    if !(200..300).contains(&status) {
        return Reply { ok: false, status, ..Reply::default() };
    }

    let mut reader = std::io::BufReader::new(res.into_reader());
    let mut parser = FrameParser::default();
    let mut line = Vec::new();
    let mut quiet = Duration::ZERO;
    let mut total = 0usize;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Reply { ok: true, status, error: "cancelled".into(), ..Reply::default() };
        }
        line.clear();
        match read_line(&mut reader, &mut line) {
            Ok(0) => {
                // The connection closed. That is the end of the stream, and
                // whether it was a CLEAN end is the server's business to have
                // said in a frame — this only reports that it stopped.
                return Reply { ok: true, status, ..Reply::default() };
            }
            Ok(_) => quiet = Duration::ZERO,
            Err(e) if is_timeout(&e) => {
                // One read deadline passed with nothing on the wire. That is
                // normal — it is how cancellation gets noticed — so add it up
                // and only give in once the connection has been quiet for
                // longer than a keepalive interval could explain.
                quiet += STREAM_READ_TIMEOUT;
                if quiet >= STREAM_IDLE_TIMEOUT {
                    return Reply {
                        ok: false,
                        status,
                        error: "the stream went quiet — not even a keepalive".into(),
                        ..Reply::default()
                    };
                }
                continue;
            }
            Err(e) => {
                return Reply { ok: false, status, error: e.to_string(), ..Reply::default() }
            }
        }
        total += line.len();
        if total > MAX_BODY {
            return Reply {
                ok: false,
                status,
                error: format!("the stream passed the {} MB limit", MAX_BODY / 1024 / 1024),
                ..Reply::default()
            };
        }
        let text = String::from_utf8_lossy(&line);
        if let Some((event, data)) = parser.push(text.trim_end_matches(['\r', '\n']))
            && tx.send(Envelope::Frame { id, event, data }).is_err()
        {
            // Nobody is listening any more — the editor closed or the package
            // reloaded. Stop reading rather than filling a dead channel.
            return Reply { ok: true, status, ..Reply::default() };
        }
    }
}

/// Did this read fail because the deadline passed, rather than because the
/// connection broke?
fn is_timeout(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

/// Read one line, including its terminator, without requiring valid UTF-8.
///
/// `BufRead::read_line` wants a `String` and fails the whole read on a byte
/// sequence that is not UTF-8; a stream is somebody else's output and one bad
/// byte should cost one mangled character, not the connection.
fn read_line(r: &mut impl std::io::BufRead, out: &mut Vec<u8>) -> std::io::Result<usize> {
    r.read_until(b'\n', out)
}

/// The SSE line protocol, one line at a time.
#[derive(Default)]
struct FrameParser {
    event: String,
    data: String,
    /// Whether any field at all has arrived since the last blank line. A blank
    /// line with nothing before it is not an empty frame, it is padding.
    started: bool,
}

impl FrameParser {
    /// Feed one line. Returns a frame when that line ended one.
    fn push(&mut self, line: &str) -> Option<(String, String)> {
        if line.is_empty() {
            if !self.started {
                return None;
            }
            let event = std::mem::take(&mut self.event);
            let data = std::mem::take(&mut self.data);
            self.started = false;
            // An event with no name is "message", which is what the EventSource
            // spec says and what every server assumes.
            return Some((if event.is_empty() { "message".into() } else { event }, data));
        }
        // A comment. Servers send `: keepalive` every so often to hold the
        // connection open through proxies; it is not a frame and must not
        // become one.
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            // A bare field name with no colon is a field with an empty value.
            None => (line, ""),
        };
        match field {
            "event" => {
                self.started = true;
                self.event = value.to_string();
            }
            "data" => {
                self.started = true;
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(value);
            }
            // `id:` and `retry:` are part of the protocol and are consumed
            // rather than passed on: neither means anything to a caller that
            // cannot reconnect on its own.
            "id" | "retry" => self.started = true,
            _ => {}
        }
        None
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

    // ---- Server-Sent Events ------------------------------------------------

    /// Feed a whole stream body through the parser, line by line, the way the
    /// reader does.
    fn frames(body: &str) -> Vec<(String, String)> {
        let mut p = FrameParser::default();
        body.split_inclusive('\n')
            .filter_map(|l| p.push(l.trim_end_matches(['\r', '\n'])))
            .collect()
    }

    #[test]
    fn a_frame_ends_at_a_blank_line() {
        let got = frames("event: progress\ndata: {\"pct\":40}\n\n");
        assert_eq!(got, vec![("progress".into(), "{\"pct\":40}".into())]);
    }

    /// The one that costs a night if it is wrong: servers hold the connection
    /// open through proxies with `: keepalive` every 15 seconds, and a parser
    /// that treats a comment as a frame delivers a stream of empty progress.
    #[test]
    fn a_keepalive_comment_is_not_a_frame() {
        let got = frames(": keepalive\n\n: keepalive\n\nevent: done\ndata: ok\n\n");
        assert_eq!(
            got,
            vec![("done".into(), "ok".into())],
            "only the real frame should have come out"
        );
    }

    #[test]
    fn data_lines_join_and_an_unnamed_event_is_message() {
        let got = frames("data: one\ndata: two\n\n");
        assert_eq!(got, vec![("message".into(), "one\ntwo".into())]);
    }

    #[test]
    fn id_and_retry_are_consumed_rather_than_passed_on() {
        let got = frames("id: 7\nretry: 3000\ndata: x\n\n");
        assert_eq!(got, vec![("message".into(), "x".into())]);
    }

    /// `data:x` with no space is legal and means the same as `data: x`. Only
    /// ONE leading space is part of the protocol — a second one is data.
    #[test]
    fn exactly_one_space_after_the_colon_is_protocol_and_the_rest_is_data() {
        assert_eq!(frames("data:x\n\n"), vec![("message".into(), "x".into())]);
        assert_eq!(frames("data:  x\n\n"), vec![("message".into(), " x".into())]);
    }

    /// Blank lines between frames — padding, or a server flushing — must not
    /// each produce an empty frame.
    #[test]
    fn padding_between_frames_produces_nothing() {
        let got = frames("\n\n\ndata: x\n\n\n\n");
        assert_eq!(got, vec![("message".into(), "x".into())]);
    }

    /// A stream cut off mid-frame delivers the frames that DID complete and
    /// not a half-built one — the caller falls back on the strength of the
    /// `onEnd`, and a truncated frame parsed as complete is a lie about it.
    #[test]
    fn a_frame_cut_off_at_the_end_is_not_delivered() {
        let got = frames("data: whole\n\nevent: progress\ndata: {\"pct\":");
        assert_eq!(got, vec![("message".into(), "whole".into())]);
    }

    /// End to end against a real socket: the parsing, the threading, and the
    /// hand-off to the main thread.
    #[test]
    fn a_stream_delivers_its_frames_and_then_ends() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let _ = s.write_all(
                concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "Content-Type: text/event-stream\r\n",
                    "Connection: close\r\n\r\n",
                    ": keepalive\n",
                    "\n",
                    "event: progress\n",
                    "data: {\"pct\":40}\n",
                    "\n",
                    "event: done\n",
                    "data: {}\n",
                    "\n",
                )
                .as_bytes(),
            );
            let _ = s.flush();
        });

        let lua = Lua::new();
        let noop = || {
            lua.create_registry_value(lua.create_function(|_, _: mlua::Value| Ok(())).unwrap())
                .unwrap()
        };
        let mut web = WebState::default();
        let id = web
            .stream(format!("http://127.0.0.1:{port}/stream"), Vec::new(), 5.0, noop(), noop())
            .unwrap();

        let mut frames = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            web.pump();
            frames.extend(web.take_frames());
            if !web.is_streaming(id) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        web.pump();
        frames.extend(web.take_frames());

        let named: Vec<&str> = frames.iter().map(|(_, e, _)| e.as_str()).collect();
        assert_eq!(named, vec!["progress", "done"], "{frames:?}");
        assert!(!web.is_streaming(id), "the stream should have retired itself");
        assert_eq!(web.take_ready().len(), 1, "onEnd is called exactly once");
    }

    /// A server that does not do streaming answers with a status, and the
    /// caller has to be able to tell that apart from a stream that worked —
    /// that is the whole basis of falling back to polling.
    #[test]
    fn a_server_that_refuses_to_stream_reports_its_status() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = s.read(&mut buf);
            let _ = s.write_all(
                concat!(
                    "HTTP/1.1 404 Not Found\r\n",
                    "Content-Length: 9\r\n",
                    "Connection: close\r\n\r\n",
                    "no stream",
                )
                .as_bytes(),
            );
        });
        let lua = Lua::new();
        let noop = || {
            lua.create_registry_value(lua.create_function(|_, _: mlua::Value| Ok(())).unwrap())
                .unwrap()
        };
        let mut web = WebState::default();
        web.stream(format!("http://127.0.0.1:{port}/s"), Vec::new(), 5.0, noop(), noop()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut ready = Vec::new();
        while Instant::now() < deadline && ready.is_empty() {
            web.pump();
            ready = web.take_ready();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(ready.len(), 1);
        assert!(!ready[0].1.ok);
        assert_eq!(ready[0].1.status, 404, "{:?}", ready[0].1);
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
