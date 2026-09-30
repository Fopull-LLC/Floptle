//! The WebSocket leg: how a browser reaches a relay.
//!
//! A page has no UDP socket, so it cannot speak QUIC. It can open a
//! WebSocket, and a relay started with a WebSocket port accepts one beside
//! its QUIC port, so a browser player and a desktop player meet in the same
//! lobby. The relay never learns which leg a peer came in on; each is a
//! [`PeerId`] behind the [`Transport`] trait.
//!
//! **Why WebSocket, and not WebTransport.** WebTransport is QUIC from a page
//! and would carry unreliable datagrams, but its server side needs an HTTP/3
//! stack that is still settling in Rust, and a browser without it would have
//! no way in at all. A WebSocket works in every browser and behind every
//! proxy. What it costs: there is no unreliable channel, so everything rides
//! one ordered stream and a lost packet delays what follows instead of being
//! dropped. Snapshots still arrive; they arrive late under loss. WebTransport
//! can come later as another leg under the same trait.
//!
//! One WebSocket binary message is one transport message: `[channel u8]` then
//! the payload. The channel tag travels so the far end hands the session the
//! channel it was sent on.
//!
//! - [`WsServer`] (desktop): the relay's listener, `ws://` or `wss://`.
//! - [`WsClient`]: on the desktop, for tests and tools; in a browser, over
//!   `web_sys::WebSocket`.

use crate::transport::{Channel, Incoming, LinkStats, PeerId, Transport};

/// Ids a WebSocket peer gets on a server that also takes QUIC peers: far
/// above anything a QUIC server counts to, so the two never meet.
pub const WS_PEER_BASE: PeerId = 1 << 48;

/// The largest message either end accepts. The relay's own per-message caps
/// are smaller; this only stops a stream from asking for an unbounded buffer.
pub const WS_MAX_MESSAGE: usize = 1 << 20;

fn tag(c: Channel) -> u8 {
    match c {
        Channel::Reliable => 0,
        Channel::Unreliable => 1,
        Channel::UnreliableSequenced => 2,
    }
}

fn channel(t: u8) -> Channel {
    match t {
        0 => Channel::Reliable,
        2 => Channel::UnreliableSequenced,
        _ => Channel::Unreliable,
    }
}

/// `[channel][payload]`.
fn frame(ch: Channel, bytes: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(bytes.len() + 1);
    b.push(tag(ch));
    b.extend_from_slice(bytes);
    b
}

/// The other way; `None` for an empty message, which no sender makes.
fn unframe(b: &[u8]) -> Option<(Channel, Vec<u8>)> {
    let (&t, rest) = b.split_first()?;
    Some((channel(t), rest.to_vec()))
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::{WsClient, WsServer};
#[cfg(target_arch = "wasm32")]
pub use web::WsClient;

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::mpsc::{Receiver, Sender};
    use std::sync::{Arc, Mutex};

    use futures_util::{SinkExt as _, StreamExt as _};
    use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
    use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
    use tokio_tungstenite::tungstenite::Message;

    use super::*;

    /// How often the server pings a quiet peer. A browser answers a ping by
    /// itself, so a page that has nothing to say still says something.
    const PING_EVERY: std::time::Duration = std::time::Duration::from_secs(2);
    /// How long a peer may say nothing at all before it is taken as gone —
    /// the QUIC leg's idle rule, give or take, so a closed laptop leaves a
    /// lobby as quickly on either leg.
    const SILENCE: std::time::Duration = std::time::Duration::from_secs(10);
    /// How long a new connection has to finish TLS and the WebSocket upgrade.
    /// A port on the open internet meets connections that never do.
    const HANDSHAKE: std::time::Duration = std::time::Duration::from_secs(10);

    fn ws_config() -> WebSocketConfig {
        WebSocketConfig::default()
            .max_message_size(Some(WS_MAX_MESSAGE))
            .max_frame_size(Some(WS_MAX_MESSAGE))
    }

    fn runtime(name: &str) -> Result<tokio::runtime::Runtime, String> {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name(name)
            .enable_all()
            .build()
            .map_err(|e| format!("could not start the {name} runtime: {e}"))
    }

    struct Peer {
        tx: UnboundedSender<Option<Vec<u8>>>,
        addr: SocketAddr,
    }

    type Peers = Arc<Mutex<HashMap<PeerId, Peer>>>;
    type Tls = Arc<Mutex<Option<Arc<rustls::ServerConfig>>>>;

    /// A relay's WebSocket listener. Peers are numbered from
    /// [`WS_PEER_BASE`].
    pub struct WsServer {
        _rt: tokio::runtime::Runtime,
        events: Receiver<Incoming>,
        peers: Peers,
        local: SocketAddr,
        tls: Tls,
    }

    impl WsServer {
        /// Listen on `addr` (`"0.0.0.0:7789"`). With `tls`, every connection
        /// is `wss://`; without, `ws://` — what a relay behind a proxy that
        /// ends TLS for it wants, and what a page served over plain `http:`
        /// (a local test) can reach.
        pub fn bind(addr: &str, tls: Option<Arc<rustls::ServerConfig>>) -> Result<Self, String> {
            let rt = runtime("floptle-ws")?;
            let listener = rt
                .block_on(tokio::net::TcpListener::bind(addr))
                .map_err(|e| format!("could not listen for WebSockets on {addr}: {e}"))?;
            let local = listener.local_addr().map_err(|e| e.to_string())?;
            let (etx, events) = std::sync::mpsc::channel();
            let peers: Peers = Arc::default();
            let tls: Tls = Arc::new(Mutex::new(tls));
            let (p, t) = (peers.clone(), tls.clone());
            rt.spawn(async move {
                let mut next = WS_PEER_BASE;
                loop {
                    let Ok((stream, addr)) = listener.accept().await else { continue };
                    let _ = stream.set_nodelay(true);
                    next += 1;
                    let tls = t.lock().unwrap().clone();
                    tokio::spawn(serve(stream, addr, next, tls, p.clone(), etx.clone()));
                }
            });
            Ok(Self { _rt: rt, events, peers, local, tls })
        }

        pub fn local_addr(&self) -> SocketAddr {
            self.local
        }

        /// The address a peer connected from — the per-address limits key on
        /// it. Behind a proxy it is the proxy's.
        pub fn remote_addr(&self, peer: PeerId) -> Option<SocketAddr> {
            self.peers.lock().unwrap().get(&peer).map(|p| p.addr)
        }

        /// Present a new certificate to new connections (a renewal). Ones
        /// already open keep the session they made.
        pub fn set_tls(&self, tls: Option<Arc<rustls::ServerConfig>>) {
            *self.tls.lock().unwrap() = tls;
        }
    }

    async fn serve(
        stream: tokio::net::TcpStream,
        addr: SocketAddr,
        id: PeerId,
        tls: Option<Arc<rustls::ServerConfig>>,
        peers: Peers,
        events: Sender<Incoming>,
    ) {
        match tls {
            Some(cfg) => {
                let accept = tokio_rustls::TlsAcceptor::from(cfg).accept(stream);
                let Ok(Ok(s)) = tokio::time::timeout(HANDSHAKE, accept).await else { return };
                run(s, addr, id, peers, events).await
            }
            None => run(stream, addr, id, peers, events).await,
        }
    }

    async fn run<S>(stream: S, addr: SocketAddr, id: PeerId, peers: Peers, events: Sender<Incoming>)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let upgrade = tokio_tungstenite::accept_async_with_config(stream, Some(ws_config()));
        let Ok(Ok(ws)) = tokio::time::timeout(HANDSHAKE, upgrade).await else {
            return;
        };
        let (mut sink, mut source) = ws.split();
        let (tx, mut rx) = unbounded_channel::<Option<Vec<u8>>>();
        peers.lock().unwrap().insert(id, Peer { tx, addr });
        let _ = events.send(Incoming::Connected(id));
        let writer = tokio::spawn(async move {
            loop {
                let sent = match tokio::time::timeout(PING_EVERY, rx.recv()).await {
                    Ok(Some(Some(b))) => sink.send(Message::Binary(b.into())).await,
                    Ok(Some(None)) | Ok(None) => {
                        let _ = sink.close().await;
                        break;
                    }
                    Err(_) => sink.send(Message::Ping(Vec::new().into())).await,
                };
                if sent.is_err() {
                    break;
                }
            }
        });
        let mut why = None;
        loop {
            let msg = match tokio::time::timeout(SILENCE, source.next()).await {
                Ok(Some(m)) => m,
                Ok(None) => break,
                Err(_) => {
                    why = Some(format!("nothing heard for {} seconds", SILENCE.as_secs()));
                    break;
                }
            };
            match msg {
                Ok(Message::Binary(b)) => {
                    if let Some((ch, bytes)) = unframe(&b) {
                        let _ = events.send(Incoming::Message(id, ch, bytes));
                    }
                }
                Ok(Message::Close(_)) => break,
                Ok(_) => {}
                Err(e) => {
                    why = Some(e.to_string());
                    break;
                }
            }
        }
        writer.abort();
        // A kick already removed it; only a peer that left on its own is
        // reported here.
        if peers.lock().unwrap().remove(&id).is_some() {
            let _ = events.send(Incoming::Disconnected(id, why));
        }
    }

    impl Transport for WsServer {
        fn send(&mut self, peer: PeerId, channel: Channel, bytes: &[u8]) {
            if let Some(p) = self.peers.lock().unwrap().get(&peer) {
                let _ = p.tx.send(Some(frame(channel, bytes)));
            }
        }

        fn poll(&mut self) -> Vec<Incoming> {
            self.events.try_iter().collect()
        }

        fn stats(&self, _peer: PeerId) -> LinkStats {
            // TCP keeps its round trip to itself; the session measures its
            // own end to end.
            LinkStats { rtt_ms: 0.0, loss: 0.0 }
        }

        fn disconnect(&mut self, peer: PeerId) {
            if let Some(p) = self.peers.lock().unwrap().remove(&peer) {
                let _ = p.tx.send(None);
            }
        }
    }

    impl Drop for WsServer {
        fn drop(&mut self) {
            for p in self.peers.lock().unwrap().values() {
                let _ = p.tx.send(None);
            }
        }
    }

    /// A desktop WebSocket client: what the tests and tools use to reach a
    /// relay's WebSocket leg the way a page does. Plain `ws://` only.
    pub struct WsClient {
        _rt: tokio::runtime::Runtime,
        tx: UnboundedSender<Option<Vec<u8>>>,
        events: Receiver<Incoming>,
    }

    impl WsClient {
        /// Start connecting to `url` (`ws://host:port/`). Non-blocking: what
        /// is sent before the connection opens is sent once it does, in order.
        pub fn connect(url: &str) -> Result<Self, String> {
            let rt = runtime("floptle-ws-client")?;
            let (etx, events) = std::sync::mpsc::channel();
            let (tx, mut rx) = unbounded_channel::<Option<Vec<u8>>>();
            let url = url.to_string();
            rt.spawn(async move {
                let host = url.trim_start_matches("ws://").split('/').next().unwrap_or("").to_string();
                let stream = match tokio::net::TcpStream::connect(&host).await {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = etx.send(Incoming::Disconnected(crate::SERVER, Some(format!("could not reach {url}: {e}"))));
                        return;
                    }
                };
                let ws = match tokio_tungstenite::client_async_with_config(url.as_str(), stream, Some(ws_config())).await {
                    Ok((ws, _)) => ws,
                    Err(e) => {
                        let _ = etx.send(Incoming::Disconnected(crate::SERVER, Some(e.to_string())));
                        return;
                    }
                };
                let _ = etx.send(Incoming::Connected(crate::SERVER));
                let (mut sink, mut source) = ws.split();
                let writer = tokio::spawn(async move {
                    while let Some(Some(b)) = rx.recv().await {
                        if sink.send(Message::Binary(b.into())).await.is_err() {
                            break;
                        }
                    }
                    let _ = sink.close().await;
                });
                let mut why = None;
                while let Some(msg) = source.next().await {
                    match msg {
                        Ok(Message::Binary(b)) => {
                            if let Some((ch, bytes)) = unframe(&b) {
                                let _ = etx.send(Incoming::Message(crate::SERVER, ch, bytes));
                            }
                        }
                        Ok(Message::Close(_)) => break,
                        Ok(_) => {}
                        Err(e) => {
                            why = Some(e.to_string());
                            break;
                        }
                    }
                }
                writer.abort();
                let _ = etx.send(Incoming::Disconnected(crate::SERVER, why));
            });
            Ok(Self { _rt: rt, tx, events })
        }
    }

    impl Transport for WsClient {
        fn send(&mut self, _peer: PeerId, channel: Channel, bytes: &[u8]) {
            let _ = self.tx.send(Some(frame(channel, bytes)));
        }

        fn poll(&mut self) -> Vec<Incoming> {
            self.events.try_iter().collect()
        }

        fn stats(&self, _peer: PeerId) -> LinkStats {
            LinkStats { rtt_ms: 0.0, loss: 0.0 }
        }
    }

    impl Drop for WsClient {
        fn drop(&mut self) {
            let _ = self.tx.send(None);
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod web {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::JsCast;

    use super::*;

    #[derive(Default)]
    struct Shared {
        open: bool,
        closed: bool,
        /// Sent before the socket opened; flushed, in order, when it does.
        waiting: Vec<Vec<u8>>,
        events: VecDeque<Incoming>,
    }

    /// A page's WebSocket to a relay. The host appears as [`crate::SERVER`].
    pub struct WsClient {
        ws: web_sys::WebSocket,
        shared: Rc<RefCell<Shared>>,
        // Kept alive for as long as the socket can call them.
        _on_open: Closure<dyn FnMut(web_sys::Event)>,
        _on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
        _on_close: Closure<dyn FnMut(web_sys::CloseEvent)>,
    }

    // SAFETY: a browser build runs on one thread — the engine's web export
    // turns threads off, and this impl is compiled only when the target has
    // no atomics, so no second thread can exist to receive this. `Transport`
    // asks for `Send` because the desktop moves transports between threads.
    #[cfg(not(target_feature = "atomics"))]
    unsafe impl Send for WsClient {}

    impl WsClient {
        /// Start connecting to `url` (`wss://host:port/`, or `ws://` from a
        /// page served over plain `http:`). Non-blocking.
        pub fn connect(url: &str) -> Result<Self, String> {
            let ws = web_sys::WebSocket::new(url).map_err(|e| {
                format!("could not open a WebSocket to {url}: {}", e.as_string().unwrap_or_default())
            })?;
            ws.set_binary_type(web_sys::BinaryType::Arraybuffer);
            let shared: Rc<RefCell<Shared>> = Rc::default();

            let (s, w) = (shared.clone(), ws.clone());
            let on_open = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
                let mut s = s.borrow_mut();
                s.open = true;
                for b in std::mem::take(&mut s.waiting) {
                    let _ = w.send_with_u8_array(&b);
                }
                s.events.push_back(Incoming::Connected(crate::SERVER));
            });
            let s = shared.clone();
            let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
                if let Ok(buf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                    let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                    if let Some((ch, payload)) = unframe(&bytes) {
                        s.borrow_mut().events.push_back(Incoming::Message(crate::SERVER, ch, payload));
                    }
                }
            });
            let (s, u) = (shared.clone(), url.to_string());
            let on_close = Closure::<dyn FnMut(web_sys::CloseEvent)>::new(move |e: web_sys::CloseEvent| {
                let mut s = s.borrow_mut();
                if s.closed {
                    return;
                }
                s.closed = true;
                // A browser says nothing about why a socket never opened —
                // the likely reasons are the address and the certificate.
                let why = if !s.open {
                    format!(
                        "could not connect to {u} — check the address, and that the relay's \
                         certificate is valid for it (a page will not accept a self-signed one)"
                    )
                } else if e.reason().is_empty() {
                    format!("the connection closed (code {})", e.code())
                } else {
                    e.reason()
                };
                s.events.push_back(Incoming::Disconnected(crate::SERVER, Some(why)));
            });
            ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
            ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
            ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
            Ok(Self { ws, shared, _on_open: on_open, _on_message: on_message, _on_close: on_close })
        }
    }

    impl Transport for WsClient {
        fn send(&mut self, _peer: PeerId, channel: Channel, bytes: &[u8]) {
            let b = frame(channel, bytes);
            let mut s = self.shared.borrow_mut();
            if s.closed {
                return;
            }
            if s.open {
                let _ = self.ws.send_with_u8_array(&b);
            } else {
                s.waiting.push(b);
            }
        }

        fn poll(&mut self) -> Vec<Incoming> {
            self.shared.borrow_mut().events.drain(..).collect()
        }

        fn stats(&self, _peer: PeerId) -> LinkStats {
            LinkStats { rtt_ms: 0.0, loss: 0.0 }
        }
    }

    impl Drop for WsClient {
        fn drop(&mut self) {
            self.ws.set_onopen(None);
            self.ws.set_onmessage(None);
            self.ws.set_onclose(None);
            let _ = self.ws.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A peer that finishes the upgrade and then never answers a ping (a
    /// closed laptop, a pulled cable) is reported gone, as the QUIC leg
    /// reports a vanished peer — not held in its lobby forever.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_peer_that_goes_silent_is_reported_gone() {
        use std::io::{Read as _, Write as _};
        let mut server = WsServer::bind("127.0.0.1:0", None).unwrap();
        let port = server.local_addr().port();
        // A hand-made upgrade over a plain socket that is then never read:
        // nothing answers the server's pings.
        let mut sock = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            sock,
            "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        .unwrap();
        let mut reply = [0u8; 12];
        sock.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"HTTP/1.1 101");
        let start = std::time::Instant::now();
        let mut events = Vec::new();
        while start.elapsed() < std::time::Duration::from_secs(20) {
            events.extend(server.poll());
            if events.iter().any(|e| matches!(e, Incoming::Disconnected(..))) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(matches!(events.first(), Some(Incoming::Connected(p)) if *p > WS_PEER_BASE), "{events:?}");
        let why = events.iter().find_map(|e| match e {
            Incoming::Disconnected(_, why) => Some(why.clone()),
            _ => None,
        });
        assert!(
            why.as_ref().is_some_and(|w| w.as_deref().is_some_and(|w| w.contains("nothing heard"))),
            "a silent peer was never reported gone: {events:?}"
        );
        drop(sock);
    }

    #[test]
    fn a_frame_carries_its_channel() {
        for ch in [Channel::Reliable, Channel::Unreliable, Channel::UnreliableSequenced] {
            assert_eq!(unframe(&frame(ch, b"hi")), Some((ch, b"hi".to_vec())));
        }
        assert_eq!(unframe(&[]), None);
    }
}
