//! The QUIC transport (phase 2e, `docs/netcode-design.md` §5.3/§10): the same
//! [`Transport`] seam the sessions already speak, over a real network.
//!
//! quinn runs on a small background tokio runtime; the sync game loop talks to
//! it through channels, so the editor/runtime never awaits anything:
//!
//! - [`Channel::Reliable`] rides one ordered unidirectional QUIC stream per
//!   direction, length-prefix framed (control: hello/spawn/rpc).
//! - [`Channel::Unreliable`] / [`Channel::UnreliableSequenced`] ride QUIC
//!   datagrams tagged `[tag u8][seq u64 LE][payload]`; the receiver drops
//!   stale sequenced datagrams. A datagram too large for the path MTU falls
//!   back to the reliable stream (correct, just not droppable — the 2e
//!   interest/byte-budget work keeps snapshots under the MTU).
//!
//! **Dev-trust security model (v1):** the server presents a fresh self-signed
//! certificate and clients accept ANY certificate. That makes LAN/self-hosted
//! play zero-config, and it is exactly as trustworthy as a Minecraft server —
//! the connection is encrypted, but the server's identity is not verified.
//! Verified identity (real certs on relay/Cloud hosts) lands with
//! `floptle-relay`.

use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use crate::transport::{Channel, Incoming, LinkStats, PeerId, Transport, SERVER};

/// Datagram tags (first byte on the wire).
const DGRAM_UNRELIABLE: u8 = 1;
const DGRAM_SEQUENCED: u8 = 2;

/// Reliable-stream frame cap — a decoder guard, far above any real message
/// (RPC/values are ≤ 1 KB by the §13.2 guardrails; spawns are small RON docs).
const MAX_FRAME: usize = 1 << 20;

/// Keep-alives + a short idle timeout so a vanished peer is detected in
/// seconds, not minutes.
const KEEP_ALIVE: Duration = Duration::from_millis(500);
const IDLE_TIMEOUT: Duration = Duration::from_secs(8);

fn install_crypto_provider() {
    // Idempotent: fails harmlessly if another component installed one already.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn transport_config() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(KEEP_ALIVE));
    t.max_idle_timeout(Some(IDLE_TIMEOUT.try_into().expect("valid idle timeout")));
    t
}

/// One live peer connection, shared with the sync `send` path.
struct PeerHandle {
    conn: quinn::Connection,
    /// Reliable messages queue here; a writer task frames them onto the stream.
    reliable: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Datagram sequence counter (both unreliable tags share it — monotonic is
    /// all the sequenced receiver needs).
    seq: AtomicU64,
}

impl PeerHandle {
    fn new(conn: quinn::Connection) -> (Self, tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self { conn, reliable: tx, seq: AtomicU64::new(0) }, rx)
    }

    /// The sync send path (called from the game loop).
    fn send(&self, channel: Channel, bytes: &[u8]) {
        match channel {
            Channel::Reliable => {
                let _ = self.reliable.send(bytes.to_vec());
            }
            Channel::Unreliable | Channel::UnreliableSequenced => {
                let tag = if channel == Channel::UnreliableSequenced {
                    DGRAM_SEQUENCED
                } else {
                    DGRAM_UNRELIABLE
                };
                let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
                let mut b = Vec::with_capacity(9 + bytes.len());
                b.push(tag);
                b.extend_from_slice(&seq.to_le_bytes());
                b.extend_from_slice(bytes);
                let fits =
                    self.conn.max_datagram_size().is_some_and(|max| b.len() <= max);
                if !fits || self.conn.send_datagram(b.into()).is_err() {
                    // Too large for the path MTU (or congestion-blocked): the
                    // reliable stream is the honest fallback — delivered, just
                    // not droppable.
                    let _ = self.reliable.send(bytes.to_vec());
                }
            }
        }
    }
}

/// Drain the reliable outbox onto the stream, length-prefix framed.
async fn write_frames(
    mut tx: quinn::SendStream,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
) {
    while let Some(b) = rx.recv().await {
        let len = (b.len() as u32).to_le_bytes();
        if tx.write_all(&len).await.is_err() || tx.write_all(&b).await.is_err() {
            return;
        }
    }
}

/// Read length-prefixed frames off the peer's reliable stream.
async fn read_frames(mut rx: quinn::RecvStream, peer: PeerId, events: mpsc::Sender<Incoming>) {
    loop {
        let mut len = [0u8; 4];
        if rx.read_exact(&mut len).await.is_err() {
            return;
        }
        let n = u32::from_le_bytes(len) as usize;
        if n > MAX_FRAME {
            return; // corrupt/hostile framing: drop the stream
        }
        let mut buf = vec![0u8; n];
        if rx.read_exact(&mut buf).await.is_err() {
            return;
        }
        if events.send(Incoming::Message(peer, Channel::Reliable, buf)).is_err() {
            return;
        }
    }
}

/// Receive datagrams; sequenced ones drop when stale.
async fn read_datagrams(conn: quinn::Connection, peer: PeerId, events: mpsc::Sender<Incoming>) {
    let mut last_seq = 0u64;
    loop {
        let Ok(d) = conn.read_datagram().await else { return };
        if d.len() < 9 {
            continue;
        }
        let tag = d[0];
        let seq = u64::from_le_bytes(d[1..9].try_into().expect("9-byte header"));
        let channel = match tag {
            DGRAM_SEQUENCED => {
                if seq <= last_seq {
                    continue; // stale — a newer one already delivered
                }
                last_seq = seq;
                Channel::UnreliableSequenced
            }
            _ => Channel::Unreliable,
        };
        if events.send(Incoming::Message(peer, channel, d[9..].to_vec())).is_err() {
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// The authoritative host's endpoint: accepts QUIC clients, assigns peer ids
/// from 1 up. Create with [`QuicServer::bind`]; drop to shut down.
pub struct QuicServer {
    runtime: Option<tokio::runtime::Runtime>,
    events: mpsc::Receiver<Incoming>,
    peers: Arc<Mutex<HashMap<PeerId, Arc<PeerHandle>>>>,
    port: u16,
}

impl QuicServer {
    /// Bind on `0.0.0.0:port` (0 = ephemeral, see [`Self::local_port`]) with a
    /// fresh self-signed certificate (see the module docs' security model).
    pub fn bind(port: u16) -> Result<Self, String> {
        install_crypto_provider();
        let cert = rcgen::generate_simple_self_signed(vec!["floptle-dev".into()])
            .map_err(|e| format!("self-signed cert: {e}"))?;
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()),
        );
        let chain = vec![cert.cert.der().clone()];
        let mut server_config = quinn::ServerConfig::with_single_cert(chain, key)
            .map_err(|e| format!("server tls: {e}"))?;
        server_config.transport_config(Arc::new(transport_config()));

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| format!("net runtime: {e}"))?;
        let addr: SocketAddr = ([0, 0, 0, 0], port).into();
        let endpoint = runtime
            .block_on(async { quinn::Endpoint::server(server_config, addr) })
            .map_err(|e| format!("bind {addr}: {e}"))?;
        let local_port = endpoint.local_addr().map_err(|e| e.to_string())?.port();

        let (events_tx, events_rx) = mpsc::channel();
        let peers: Arc<Mutex<HashMap<PeerId, Arc<PeerHandle>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        {
            let peers = peers.clone();
            runtime.spawn(async move {
                let mut next_peer: PeerId = 1;
                while let Some(incoming) = endpoint.accept().await {
                    let Ok(conn) = incoming.await else { continue };
                    let peer = next_peer;
                    next_peer += 1;
                    let (handle, reliable_rx) = PeerHandle::new(conn.clone());
                    peers.lock().unwrap().insert(peer, Arc::new(handle));
                    if events_tx.send(Incoming::Connected(peer)).is_err() {
                        return; // transport dropped
                    }
                    // Writer: our reliable stream toward this peer.
                    let c = conn.clone();
                    let rrx = reliable_rx;
                    tokio::spawn(async move {
                        let Ok(tx) = c.open_uni().await else { return };
                        write_frames(tx, rrx).await;
                    });
                    // Reader: the peer's reliable stream toward us.
                    let c = conn.clone();
                    let ev = events_tx.clone();
                    tokio::spawn(async move {
                        let Ok(rx) = c.accept_uni().await else { return };
                        read_frames(rx, peer, ev).await;
                    });
                    // Datagrams.
                    tokio::spawn(read_datagrams(conn.clone(), peer, events_tx.clone()));
                    // Death watch.
                    let ev = events_tx.clone();
                    let peers = peers.clone();
                    tokio::spawn(async move {
                        conn.closed().await;
                        peers.lock().unwrap().remove(&peer);
                        let _ = ev.send(Incoming::Disconnected(peer));
                    });
                }
            });
        }
        Ok(Self { runtime: Some(runtime), events: events_rx, peers, port: local_port })
    }

    /// The actually-bound UDP port (useful with `bind(0)`).
    pub fn local_port(&self) -> u16 {
        self.port
    }
}

impl Transport for QuicServer {
    fn send(&mut self, peer: PeerId, channel: Channel, bytes: &[u8]) {
        let handle = self.peers.lock().unwrap().get(&peer).cloned();
        if let Some(h) = handle {
            h.send(channel, bytes);
        }
    }

    fn poll(&mut self) -> Vec<Incoming> {
        self.events.try_iter().collect()
    }

    fn stats(&self, peer: PeerId) -> LinkStats {
        let rtt = self
            .peers
            .lock()
            .unwrap()
            .get(&peer)
            .map(|h| h.conn.rtt().as_secs_f32() * 1000.0)
            .unwrap_or(0.0);
        LinkStats { rtt_ms: rtt, loss: 0.0 }
    }
}

impl Drop for QuicServer {
    fn drop(&mut self) {
        // Say goodbye properly: close every connection (CONNECTION_CLOSE goes
        // out), give the driver a beat to transmit, then drop the runtime.
        for h in self.peers.lock().unwrap().values() {
            h.conn.close(0u32.into(), b"server closed");
        }
        std::thread::sleep(Duration::from_millis(20));
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// Accept any server certificate (the dev-trust model — see the module docs).
#[derive(Debug)]
struct AcceptAnyCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// A client endpoint connecting to a [`QuicServer`]. [`QuicClient::connect`]
/// returns immediately; the handshake completes in the background (reliable
/// sends queue meanwhile — the session's `Hello` is the first thing through).
pub struct QuicClient {
    runtime: Option<tokio::runtime::Runtime>,
    events: mpsc::Receiver<Incoming>,
    conn: Arc<Mutex<Option<quinn::Connection>>>,
    reliable: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    seq: AtomicU64,
}

impl QuicClient {
    /// Connect to `host:port` (an IP or a resolvable name).
    pub fn connect(addr: &str) -> Result<Self, String> {
        install_crypto_provider();
        let remote: SocketAddr = addr
            .to_socket_addrs()
            .map_err(|e| format!("resolve {addr}: {e}"))?
            .next()
            .ok_or_else(|| format!("resolve {addr}: no address"))?;

        let provider = rustls::crypto::CryptoProvider::get_default()
            .cloned()
            .ok_or("no crypto provider")?;
        let tls = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|e| format!("tls: {e}"))?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider)))
            .with_no_client_auth();
        let mut client_config = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(tls)
                .map_err(|e| format!("quic tls: {e}"))?,
        ));
        client_config.transport_config(Arc::new(transport_config()));

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| format!("net runtime: {e}"))?;
        let bind: SocketAddr = if remote.is_ipv6() {
            "[::]:0".parse().expect("valid")
        } else {
            "0.0.0.0:0".parse().expect("valid")
        };
        let mut endpoint = runtime
            .block_on(async { quinn::Endpoint::client(bind) })
            .map_err(|e| format!("bind: {e}"))?;
        endpoint.set_default_client_config(client_config);

        let (events_tx, events_rx) = mpsc::channel();
        let (reliable_tx, reliable_rx) = tokio::sync::mpsc::unbounded_channel();
        let conn_slot: Arc<Mutex<Option<quinn::Connection>>> = Arc::new(Mutex::new(None));
        {
            let conn_slot = conn_slot.clone();
            runtime.spawn(async move {
                let connecting = match endpoint.connect(remote, "floptle-dev") {
                    Ok(c) => c,
                    Err(_) => {
                        let _ = events_tx.send(Incoming::Disconnected(SERVER));
                        return;
                    }
                };
                let Ok(conn) = connecting.await else {
                    let _ = events_tx.send(Incoming::Disconnected(SERVER));
                    return;
                };
                *conn_slot.lock().unwrap() = Some(conn.clone());
                let _ = events_tx.send(Incoming::Connected(SERVER));
                let c = conn.clone();
                tokio::spawn(async move {
                    let Ok(tx) = c.open_uni().await else { return };
                    write_frames(tx, reliable_rx).await;
                });
                let c = conn.clone();
                let ev = events_tx.clone();
                tokio::spawn(async move {
                    let Ok(rx) = c.accept_uni().await else { return };
                    read_frames(rx, SERVER, ev).await;
                });
                tokio::spawn(read_datagrams(conn.clone(), SERVER, events_tx.clone()));
                conn.closed().await;
                *conn_slot.lock().unwrap() = None;
                let _ = events_tx.send(Incoming::Disconnected(SERVER));
            });
        }
        Ok(Self {
            runtime: Some(runtime),
            events: events_rx,
            conn: conn_slot,
            reliable: reliable_tx,
            seq: AtomicU64::new(0),
        })
    }
}

impl Transport for QuicClient {
    fn send(&mut self, _peer: PeerId, channel: Channel, bytes: &[u8]) {
        match channel {
            Channel::Reliable => {
                // Queues even before the handshake completes — the writer task
                // drains once the stream opens (Hello arrives first, always).
                let _ = self.reliable.send(bytes.to_vec());
            }
            Channel::Unreliable | Channel::UnreliableSequenced => {
                let conn = self.conn.lock().unwrap().clone();
                let Some(conn) = conn else { return }; // not connected: droppable by contract
                let tag = if channel == Channel::UnreliableSequenced {
                    DGRAM_SEQUENCED
                } else {
                    DGRAM_UNRELIABLE
                };
                let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
                let mut b = Vec::with_capacity(9 + bytes.len());
                b.push(tag);
                b.extend_from_slice(&seq.to_le_bytes());
                b.extend_from_slice(bytes);
                let fits = conn.max_datagram_size().is_some_and(|max| b.len() <= max);
                if !fits || conn.send_datagram(b.into()).is_err() {
                    let _ = self.reliable.send(bytes.to_vec());
                }
            }
        }
    }

    fn poll(&mut self) -> Vec<Incoming> {
        self.events.try_iter().collect()
    }

    fn stats(&self, _peer: PeerId) -> LinkStats {
        let rtt = self
            .conn
            .lock()
            .unwrap()
            .as_ref()
            .map(|c| c.rtt().as_secs_f32() * 1000.0)
            .unwrap_or(0.0);
        LinkStats { rtt_ms: rtt, loss: 0.0 }
    }
}

impl Drop for QuicClient {
    fn drop(&mut self) {
        // Graceful goodbye so the server learns NOW, not at the idle timeout.
        if let Some(c) = self.conn.lock().unwrap().take() {
            c.close(0u32.into(), b"left");
        }
        std::thread::sleep(Duration::from_millis(20));
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Accumulates every polled event so a wait for one kind never discards
    /// another that arrived in the same batch.
    struct Polled<'a> {
        t: &'a mut dyn Transport,
        seen: Vec<Incoming>,
    }

    impl<'a> Polled<'a> {
        fn new(t: &'a mut dyn Transport) -> Self {
            Self { t, seen: Vec::new() }
        }

        /// Wait (~2 s) until `want` accumulated events match `pred`.
        fn wait_for(
            &mut self,
            mut pred: impl FnMut(&Incoming) -> bool,
            want: usize,
        ) -> Vec<Incoming> {
            for _ in 0..400 {
                self.seen.extend(self.t.poll());
                let got: Vec<Incoming> =
                    self.seen.iter().filter(|i| pred(i)).cloned().collect();
                if got.len() >= want {
                    return got;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            self.seen.iter().filter(|i| pred(i)).cloned().collect()
        }
    }

    #[test]
    fn quic_end_to_end_all_channels() {
        let mut server = QuicServer::bind(0).expect("bind");
        let port = server.local_port();
        let mut client = QuicClient::connect(&format!("127.0.0.1:{port}")).expect("connect");

        // Reliable queued BEFORE the handshake completes must still arrive first.
        client.send(SERVER, Channel::Reliable, b"hello");

        let mut on_server = Polled::new(&mut server);
        assert_eq!(
            on_server.wait_for(|i| matches!(i, Incoming::Connected(1)), 1).len(),
            1,
            "server must see the client connect"
        );
        assert_eq!(
            on_server
                .wait_for(
                    |i| matches!(i, Incoming::Message(1, Channel::Reliable, b) if b == b"hello"),
                    1,
                )
                .len(),
            1,
            "pre-handshake reliable send must arrive"
        );
        drop(on_server);

        // Client sees Connected once the handshake lands.
        let mut on_client = Polled::new(&mut client);
        assert_eq!(
            on_client.wait_for(|i| matches!(i, Incoming::Connected(SERVER)), 1).len(),
            1
        );
        drop(on_client);

        // A big reliable frame survives framing (larger than any datagram).
        let big = vec![0xAB; 64 * 1024];
        server.send(1, Channel::Reliable, &big);
        let mut on_client = Polled::new(&mut client);
        assert_eq!(
            on_client
                .wait_for(
                    |i| matches!(i, Incoming::Message(SERVER, Channel::Reliable, b) if b.len() == big.len()),
                    1,
                )
                .len(),
            1,
            "64 KiB reliable frame must arrive intact"
        );
        drop(on_client);

        // Datagrams both ways (localhost: no loss in practice).
        client.send(SERVER, Channel::Unreliable, b"dgram");
        let mut on_server = Polled::new(&mut server);
        assert_eq!(
            on_server
                .wait_for(
                    |i| matches!(i, Incoming::Message(1, Channel::Unreliable, b) if b == b"dgram"),
                    1,
                )
                .len(),
            1,
            "unreliable datagram must arrive on localhost"
        );
        drop(on_server);

        server.send(1, Channel::UnreliableSequenced, b"snap1");
        server.send(1, Channel::UnreliableSequenced, b"snap2");
        let mut on_client = Polled::new(&mut client);
        let got = on_client.wait_for(
            |i| matches!(i, Incoming::Message(SERVER, Channel::UnreliableSequenced, _)),
            2,
        );
        assert!(!got.is_empty(), "sequenced datagrams must arrive");
        // In-order arrivals all deliver; the newest must be among them.
        assert!(got.iter().any(|i| matches!(i, Incoming::Message(_, _, b) if b == b"snap2")));
        drop(on_client);

        // Dropping the client surfaces as a disconnect on the server.
        drop(client);
        let mut on_server = Polled::new(&mut server);
        assert_eq!(
            on_server.wait_for(|i| matches!(i, Incoming::Disconnected(1)), 1).len(),
            1,
            "server must notice the client vanish"
        );
    }

    #[test]
    fn a_full_session_replicates_over_quic() {
        use floptle_core::math::DVec3;
        use floptle_core::transform::Transform;
        use floptle_core::{Replicated, World};

        let world_with = |n: usize| {
            let mut w = World::default();
            let mut ents = Vec::new();
            for i in 0..n {
                let e = w.spawn();
                w.insert(e, Transform::from_translation(DVec3::new(10.0 * i as f64, 0.0, 0.0)));
                w.insert(e, Replicated::default());
                ents.push(e);
            }
            (w, ents)
        };

        let server_t = QuicServer::bind(0).expect("bind");
        let port = server_t.local_port();
        let client_t = QuicClient::connect(&format!("127.0.0.1:{port}")).expect("connect");

        let mut server = crate::NetSession::server(Box::new(server_t));
        let mut client = crate::NetSession::client(Box::new(client_t));
        let (mut sw, se) = world_with(2);
        let (mut cw, ce) = world_with(2);
        server.register_scene(&sw);
        client.register_scene(&cw);

        // Drive both sessions like a 60 Hz loop for ~1.5 s of real time.
        for t in 1..=90u64 {
            if let Some(tr) = sw.get_mut::<Transform>(se[0]) {
                tr.translation.x = t as f64 * 0.1;
            }
            server.tick_server(&sw, t);
            client.tick_client(&mut cw);
            std::thread::sleep(Duration::from_millis(15));
        }

        assert!(client.is_connected(), "the session handshake must complete over QUIC");
        let cx = cw.get::<Transform>(ce[0]).unwrap().translation.x;
        let sx = sw.get::<Transform>(se[0]).unwrap().translation.x;
        assert!(cx > 1.0, "replicated motion must reach the client, got {cx}");
        assert!(cx <= sx + 1e-9, "client renders at/behind the server, {cx} vs {sx}");

        // An RPC with a perceived-tick stamp crosses the real wire too.
        client
            .send_rpc_stamped("swing", crate::NetValue::Num(1.0), crate::RpcTarget::Server, true)
            .unwrap();
        let mut got = Vec::new();
        for t in 91..=140u64 {
            server.tick_server(&sw, t);
            client.tick_client(&mut cw);
            got.extend(server.take_rpcs());
            if !got.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "swing");
        assert_eq!(got[0].sender, 1);
        assert!(got[0].tick.is_some(), "the withInput stamp survives the wire");
    }
}
