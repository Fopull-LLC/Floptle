//! The rendezvous relay (`docs/netcode-design.md` §10): hosts register and get
//! a **lobby code**; clients join with the code; the relay forwards opaque
//! session traffic both ways. Nobody port-forwards — the only reachable
//! address anyone needs is the relay's. This is the open, self-hostable
//! reference implementation (ADR-0022); Floptle Cloud runs the managed one.
//!
//! Everything rides the QUIC transport this crate already has: an endpoint's
//! leg to the relay is an ordinary [`QuicClient`], the relay itself an
//! ordinary [`QuicServer`] — control + reliable game traffic on the framed
//! stream (ordered, so a `Join` is always processed before the session's
//! `Hello` that follows it), unreliable game traffic as datagrams.
//!
//! Sequenced-drop semantics are END-TO-END: the sender stamps a `seq` inside
//! the relayed message and the FINAL receiver drops stale ones per
//! `(peer, channel)` — the legs themselves carry unreliable datagrams without
//! per-leg dedup, so interleaved traffic for different peers can never
//! false-drop.
//!
//! [`RelayServer`] is deliberately dumb: lobbies, peer ids, forwarding. No
//! game state, no inspection — a session over a relay is the same bytes as a
//! direct one.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use crate::quic::{QuicClient, QuicServer};
use crate::transport::{Channel, Incoming, LinkStats, PeerId, Transport, SERVER};

/// Wire channel tags inside relay messages.
const CH_RELIABLE: u8 = 0;
const CH_UNRELIABLE: u8 = 1;
const CH_SEQUENCED: u8 = 2;

fn channel_tag(c: Channel) -> u8 {
    match c {
        Channel::Reliable => CH_RELIABLE,
        Channel::Unreliable => CH_UNRELIABLE,
        Channel::UnreliableSequenced => CH_SEQUENCED,
    }
}

fn tag_channel(t: u8) -> Channel {
    match t {
        CH_RELIABLE => Channel::Reliable,
        CH_SEQUENCED => Channel::UnreliableSequenced,
        _ => Channel::Unreliable,
    }
}

/// Everything that crosses a relay leg.
#[derive(Clone, Debug, Serialize, Deserialize)]
enum RelayMsg {
    /// Endpoint → relay: host a new lobby.
    Host,
    /// Endpoint → relay: join a lobby by code.
    Join { code: String },
    /// Relay → host: your lobby is live.
    Hosted { code: String },
    /// Relay → client: you're in.
    JoinOk,
    /// Relay → endpoint: no.
    Refused { reason: String },
    /// Relay → host: a client attached / detached (its game peer id).
    PeerJoined { peer: u64 },
    PeerLeft { peer: u64 },
    /// Host → relay: deliver to one client. `seq` is the end-to-end
    /// sequenced-drop stamp (0 on non-sequenced channels).
    ToPeer { peer: u64, channel: u8, seq: u64, bytes: Vec<u8> },
    /// Relay → host: a client's traffic.
    FromPeer { peer: u64, channel: u8, seq: u64, bytes: Vec<u8> },
    /// Client → relay: deliver to the host.
    ToHost { channel: u8, seq: u64, bytes: Vec<u8> },
    /// Relay → client: the host's traffic.
    FromHost { channel: u8, seq: u64, bytes: Vec<u8> },
}

impl RelayMsg {
    fn encode(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("relay messages always encode")
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        postcard::from_bytes(bytes).ok()
    }
}

/// Lobby codes: 5 characters from an unambiguous alphabet (no 0/O, 1/I).
fn lobby_code(rng: &mut u64) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    (0..5)
        .map(|_| {
            let mut x = *rng;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *rng = x;
            ALPHABET[(x >> 33) as usize % ALPHABET.len()] as char
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The relay server
// ---------------------------------------------------------------------------

/// What a relay connection currently is.
enum Role {
    Fresh,
    Host { code: String },
    Client { code: String, game_peer: u64 },
}

struct Lobby {
    /// The host's relay connection.
    host: PeerId,
    /// game peer id → the client's relay connection.
    clients: HashMap<u64, PeerId>,
    next_peer: u64,
}

/// The relay: step it forever (the `floptle-relay` binary) or from a test
/// thread. One instance serves many lobbies.
pub struct RelayServer {
    transport: QuicServer,
    conns: HashMap<PeerId, Role>,
    lobbies: HashMap<String, Lobby>,
    rng: u64,
    port: u16,
}

impl RelayServer {
    /// Bind on `0.0.0.0:port` (0 = ephemeral; see [`Self::port`]).
    pub fn bind(port: u16) -> Result<Self, String> {
        let transport = QuicServer::bind(port)?;
        let port = transport.local_port();
        let seed = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x5EED)
            | 1;
        Ok(Self { transport, conns: HashMap::new(), lobbies: HashMap::new(), rng: seed, port })
    }

    /// The actually-bound UDP port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Live lobby count (diagnostics).
    pub fn lobby_count(&self) -> usize {
        self.lobbies.len()
    }

    /// Process everything that arrived; returns how many messages moved.
    /// Drive it in a loop with a short sleep (the reference binary does 1 ms).
    pub fn step(&mut self) -> usize {
        let mut moved = 0;
        for inc in self.transport.poll() {
            moved += 1;
            match inc {
                Incoming::Connected(c) => {
                    self.conns.insert(c, Role::Fresh);
                }
                Incoming::Disconnected(c) => self.drop_conn(c),
                Incoming::Message(c, ch, bytes) => {
                    let Some(msg) = RelayMsg::decode(&bytes) else { continue };
                    self.dispatch(c, ch, msg);
                }
            }
        }
        moved
    }

    fn dispatch(&mut self, from: PeerId, leg_channel: Channel, msg: RelayMsg) {
        match msg {
            RelayMsg::Host => {
                let code = loop {
                    let c = lobby_code(&mut self.rng);
                    if !self.lobbies.contains_key(&c) {
                        break c;
                    }
                };
                self.lobbies.insert(
                    code.clone(),
                    Lobby { host: from, clients: HashMap::new(), next_peer: 1 },
                );
                self.conns.insert(from, Role::Host { code: code.clone() });
                self.send(from, Channel::Reliable, &RelayMsg::Hosted { code });
            }
            RelayMsg::Join { code } => {
                let Some(lobby) = self.lobbies.get_mut(&code) else {
                    self.send(
                        from,
                        Channel::Reliable,
                        &RelayMsg::Refused { reason: format!("no lobby {code}") },
                    );
                    return;
                };
                let peer = lobby.next_peer;
                lobby.next_peer += 1;
                lobby.clients.insert(peer, from);
                let host = lobby.host;
                self.conns.insert(from, Role::Client { code, game_peer: peer });
                self.send(from, Channel::Reliable, &RelayMsg::JoinOk);
                self.send(host, Channel::Reliable, &RelayMsg::PeerJoined { peer });
            }
            RelayMsg::ToPeer { peer, channel, seq, bytes } => {
                let Some(Role::Host { code }) = self.conns.get(&from) else { return };
                let Some(target) = self.lobbies.get(code).and_then(|l| l.clients.get(&peer))
                else {
                    return;
                };
                self.send(*target, leg_channel, &RelayMsg::FromHost { channel, seq, bytes });
            }
            RelayMsg::ToHost { channel, seq, bytes } => {
                let Some(Role::Client { code, game_peer }) = self.conns.get(&from) else {
                    return;
                };
                let (peer, host) = (*game_peer, self.lobbies.get(code).map(|l| l.host));
                let Some(host) = host else { return };
                self.send(host, leg_channel, &RelayMsg::FromPeer { peer, channel, seq, bytes });
            }
            _ => { /* endpoints never send the rest */ }
        }
    }

    fn drop_conn(&mut self, c: PeerId) {
        match self.conns.remove(&c) {
            Some(Role::Host { code }) => {
                // The lobby dies with its host; clients hear it as a refusal.
                if let Some(lobby) = self.lobbies.remove(&code) {
                    for (_, conn) in lobby.clients {
                        self.send(
                            conn,
                            Channel::Reliable,
                            &RelayMsg::Refused { reason: "host left".into() },
                        );
                    }
                }
            }
            Some(Role::Client { code, game_peer }) => {
                if let Some(lobby) = self.lobbies.get_mut(&code) {
                    lobby.clients.remove(&game_peer);
                    let host = lobby.host;
                    self.send(host, Channel::Reliable, &RelayMsg::PeerLeft { peer: game_peer });
                }
            }
            _ => {}
        }
    }

    fn send(&mut self, to: PeerId, channel: Channel, msg: &RelayMsg) {
        // Legs never use per-leg sequenced dedup (see the module docs) —
        // unreliable stays unreliable, sequencing is end-to-end.
        let ch = if channel == Channel::Reliable { Channel::Reliable } else { Channel::Unreliable };
        self.transport.send(to, ch, &msg.encode());
    }
}

// ---------------------------------------------------------------------------
// Endpoint transports
// ---------------------------------------------------------------------------

/// End-to-end sequenced-drop state: last seq delivered per (peer, channel).
#[derive(Default)]
struct SeqState {
    last: HashMap<(u64, u8), u64>,
}

impl SeqState {
    /// True when the message should be DROPPED (stale sequenced).
    fn stale(&mut self, peer: u64, channel: u8, seq: u64) -> bool {
        if channel != CH_SEQUENCED {
            return false;
        }
        let last = self.last.entry((peer, channel)).or_insert(0);
        if seq <= *last {
            return true;
        }
        *last = seq;
        false
    }
}

/// The HOST's end of a relayed session: one QUIC leg to the relay, a lobby
/// code for friends, and the same [`Transport`] the sessions already speak —
/// peers appear exactly as if they had connected directly.
pub struct RelayHost {
    inner: QuicClient,
    code: Option<String>,
    seq: u64,
    dedup: SeqState,
}

impl RelayHost {
    /// Connect to a relay and host a lobby. Blocks briefly (≤ ~3 s) for the
    /// lobby code — one click, one code.
    pub fn host(relay_addr: &str) -> Result<(Self, String), String> {
        let mut inner = QuicClient::connect(relay_addr)?;
        inner.send(SERVER, Channel::Reliable, &RelayMsg::Host.encode());
        let mut me =
            Self { inner, code: None, seq: 0, dedup: SeqState::default() };
        for _ in 0..600 {
            let _ = me.poll(); // stashes Hosted{code} when it lands
            if let Some(c) = me.code.clone() {
                return Ok((me, c));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Err(format!("relay {relay_addr}: no lobby code (is a relay running there?)"))
    }

    /// The lobby code (known after [`Self::host`] returns).
    pub fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }
}

impl Transport for RelayHost {
    fn send(&mut self, peer: PeerId, channel: Channel, bytes: &[u8]) {
        let seq = if channel == Channel::UnreliableSequenced {
            self.seq += 1;
            self.seq
        } else {
            0
        };
        let msg = RelayMsg::ToPeer { peer, channel: channel_tag(channel), seq, bytes: bytes.to_vec() };
        let leg = if channel == Channel::Reliable { Channel::Reliable } else { Channel::Unreliable };
        self.inner.send(SERVER, leg, &msg.encode());
    }

    fn poll(&mut self) -> Vec<Incoming> {
        let mut out = Vec::new();
        for inc in self.inner.poll() {
            match inc {
                Incoming::Message(_, _, bytes) => match RelayMsg::decode(&bytes) {
                    Some(RelayMsg::Hosted { code }) => self.code = Some(code),
                    Some(RelayMsg::PeerJoined { peer }) => out.push(Incoming::Connected(peer)),
                    Some(RelayMsg::PeerLeft { peer }) => out.push(Incoming::Disconnected(peer)),
                    Some(RelayMsg::FromPeer { peer, channel, seq, bytes })
                        if !self.dedup.stale(peer, channel, seq) =>
                    {
                        out.push(Incoming::Message(peer, tag_channel(channel), bytes));
                    }
                    _ => {}
                },
                Incoming::Disconnected(_) => {
                    // The relay leg died: every player is unreachable now.
                    let peers: Vec<u64> =
                        self.dedup.last.keys().map(|(p, _)| *p).collect();
                    for p in peers {
                        out.push(Incoming::Disconnected(p));
                    }
                }
                Incoming::Connected(_) => {}
            }
        }
        out
    }

    fn stats(&self, _peer: PeerId) -> LinkStats {
        // v1: the host↔relay leg (per-player RTT needs relay cooperation).
        self.inner.stats(SERVER)
    }
}

/// A CLIENT's end of a relayed session: joins by lobby code; the host appears
/// as [`SERVER`], exactly like a direct connection.
pub struct RelayClient {
    inner: QuicClient,
    seq: u64,
    dedup: SeqState,
}

impl RelayClient {
    /// Connect to a relay and join lobby `code`. Non-blocking: the session's
    /// `Hello` rides the same ordered stream right behind the `Join`, so the
    /// handshake completes as soon as the relay lets us in ([`Incoming`]
    /// carries a `Disconnected` if it refuses).
    pub fn join(relay_addr: &str, code: &str) -> Result<Self, String> {
        let mut inner = QuicClient::connect(relay_addr)?;
        inner.send(SERVER, Channel::Reliable, &RelayMsg::Join { code: code.to_uppercase() }.encode());
        Ok(Self { inner, seq: 0, dedup: SeqState::default() })
    }
}

impl Transport for RelayClient {
    fn send(&mut self, _peer: PeerId, channel: Channel, bytes: &[u8]) {
        let seq = if channel == Channel::UnreliableSequenced {
            self.seq += 1;
            self.seq
        } else {
            0
        };
        let msg = RelayMsg::ToHost { channel: channel_tag(channel), seq, bytes: bytes.to_vec() };
        let leg = if channel == Channel::Reliable { Channel::Reliable } else { Channel::Unreliable };
        self.inner.send(SERVER, leg, &msg.encode());
    }

    fn poll(&mut self) -> Vec<Incoming> {
        let mut out = Vec::new();
        for inc in self.inner.poll() {
            match inc {
                Incoming::Message(_, _, bytes) => match RelayMsg::decode(&bytes) {
                    Some(RelayMsg::JoinOk) => out.push(Incoming::Connected(SERVER)),
                    Some(RelayMsg::Refused { reason }) => {
                        let _ = reason;
                        out.push(Incoming::Disconnected(SERVER));
                    }
                    Some(RelayMsg::FromHost { channel, seq, bytes })
                        if !self.dedup.stale(SERVER, channel, seq) =>
                    {
                        out.push(Incoming::Message(SERVER, tag_channel(channel), bytes));
                    }
                    _ => {}
                },
                Incoming::Disconnected(_) => out.push(Incoming::Disconnected(SERVER)),
                Incoming::Connected(_) => {}
            }
        }
        out
    }

    fn stats(&self, _peer: PeerId) -> LinkStats {
        self.inner.stats(SERVER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// A relay stepping on a background thread until dropped.
    struct TestRelay {
        port: u16,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl TestRelay {
        fn start() -> Self {
            let mut relay = RelayServer::bind(0).expect("relay bind");
            let port = relay.port();
            let stop = Arc::new(AtomicBool::new(false));
            let s = stop.clone();
            let thread = std::thread::spawn(move || {
                while !s.load(Ordering::Relaxed) {
                    relay.step();
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
            Self { port, stop, thread: Some(thread) }
        }
    }

    impl Drop for TestRelay {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    #[test]
    fn a_full_session_replicates_through_the_relay() {
        use floptle_core::math::DVec3;
        use floptle_core::transform::Transform;
        use floptle_core::{Replicated, World};

        let relay = TestRelay::start();
        let addr = format!("127.0.0.1:{}", relay.port);

        let (host_t, code) = RelayHost::host(&addr).expect("host via relay");
        assert_eq!(code.len(), 5, "a real lobby code: {code}");
        let client_t = RelayClient::join(&addr, &code).expect("join via relay");

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
        let mut server = crate::NetSession::server(Box::new(host_t));
        let mut client = crate::NetSession::client(Box::new(client_t));
        let (mut sw, se) = world_with(1);
        let (mut cw, ce) = world_with(1);
        server.register_scene(&sw);
        client.register_scene(&cw);

        for t in 1..=90u64 {
            if let Some(tr) = sw.get_mut::<Transform>(se[0]) {
                tr.translation.x = t as f64 * 0.1;
            }
            server.tick_server(&sw, t);
            client.tick_client(&mut cw);
            std::thread::sleep(Duration::from_millis(15));
        }
        assert!(client.is_connected(), "the session must handshake through the relay");
        let cx = cw.get::<Transform>(ce[0]).unwrap().translation.x;
        assert!(cx > 1.0, "replicated motion must arrive via the relay, got {cx}");

        // Client → server RPC crosses too, with the stamp intact.
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
        assert_eq!(got[0].sender, 1, "the relay-assigned game peer id");
        assert!(got[0].tick.is_some());
    }

    #[test]
    fn bad_codes_refuse_and_lobbies_die_with_their_host() {
        let relay = TestRelay::start();
        let addr = format!("127.0.0.1:{}", relay.port);

        // Join with a garbage code → refused (a Disconnected on the client).
        let mut nope = RelayClient::join(&addr, "XXXXX").expect("connects to the relay fine");
        let mut refused = false;
        for _ in 0..400 {
            if nope.poll().iter().any(|i| matches!(i, Incoming::Disconnected(SERVER))) {
                refused = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(refused, "a bad code must refuse");

        // A real lobby: the host vanishing kills it for the client.
        let (host_t, code) = RelayHost::host(&addr).expect("host");
        let mut client = RelayClient::join(&addr, &code).expect("join");
        let mut joined = false;
        for _ in 0..400 {
            if client.poll().iter().any(|i| matches!(i, Incoming::Connected(SERVER))) {
                joined = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(joined, "the good code must admit");
        drop(host_t);
        let mut dead = false;
        for _ in 0..400 {
            if client.poll().iter().any(|i| matches!(i, Incoming::Disconnected(SERVER))) {
                dead = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(dead, "the lobby must die with its host");
    }
}
