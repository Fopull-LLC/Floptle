//! The rendezvous relay (`docs/multiplayer.md` §10): hosts register and get
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
    /// Endpoint → relay: host a new lobby, presenting nothing.
    ///
    /// **Kept as a unit variant on purpose.** A managed relay refuses this with
    /// a sentence telling the developer to connect their project, and that
    /// message is far more use than the silence a re-shaped variant would
    /// produce: postcard indexes variants by declaration order, so widening
    /// `Host` would make every game already in the wild fail to decode here and
    /// hang with nothing said. [`RelayMsg::HostKeyed`] is appended at the end
    /// instead, which old builds never send and new ones only send when the
    /// project actually carries a key.
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
    /// Endpoint → relay: host a lobby **as a registered game**, presenting the
    /// game key from `project.ron` and the build hash if there is one.
    ///
    /// **Appended last, and that placement is the compatibility story.**
    /// Postcard numbers enum variants by declaration order, so a new relay
    /// decodes every message an old build sends exactly as before, and this one
    /// is simply a variant old relays have never heard of. The other direction
    /// — a new build meeting an OLD relay — is what
    /// [`RelayHost::host_keyed`]'s fallback is for.
    HostKeyed { key: String, build: Option<String> },
}

impl RelayMsg {
    fn encode(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("relay messages always encode")
    }

    fn decode(bytes: &[u8]) -> Option<Self> {
        postcard::from_bytes(bytes).ok()
    }
}

/// What a relay decides about a request to host.
#[derive(Clone, Debug, PartialEq)]
pub enum HostAdmission {
    /// Allow. `prefix` is prepended to the lobby code — a managed relay puts
    /// its region letter there so a client can map a code back to a relay
    /// without asking anybody. `None` keeps the self-hosted 5-character code.
    Allow { prefix: Option<char> },
    /// Refuse, with a sentence the player reads **verbatim**. It reaches them
    /// through `Refused { reason }` → the F1 menu and `net.on("refused")`, so
    /// it is product copy, not a diagnostic.
    Refuse { reason: String },
}

/// What a relay decides about a request to join an existing lobby.
#[derive(Clone, Debug, PartialEq)]
pub enum JoinAdmission {
    Allow,
    Refuse { reason: String },
}

/// The relay's admission policy — who may host, who may join, and what the
/// lobby codes look like.
///
/// **`floptle-net` never makes a network call, and this trait is why.** A
/// managed relay decides from a key snapshot it pulls on its own schedule; a
/// self-hosted relay has no policy at all and is byte-identical to the day
/// this file was written. Keeping the decision behind a trait is what lets
/// both of those be true at once, and it is what makes the managed rules
/// testable without a network: the guards below drive a policy that answers
/// from a table.
///
/// The bookkeeping hooks default to doing nothing, so a policy that only cares
/// about admission implements one method.
pub trait RelayPolicy: Send {
    /// A host is asking for a lobby. `key` is `None` when the endpoint sent the
    /// keyless [`RelayMsg::Host`] — which a managed relay must refuse and a
    /// self-hosted one must not care about.
    fn admit_host(&mut self, key: Option<&str>, build: Option<&str>) -> HostAdmission;

    /// A client is joining `code`. This is where a CCU cap bites, and it must
    /// bite here rather than on the host: **a live session is never broken for
    /// a cap**, so the only thing a limit can do is refuse the next arrival.
    fn admit_join(&mut self, _code: &str) -> JoinAdmission {
        JoinAdmission::Allow
    }

    /// A lobby opened, under the key that was admitted. The policy owns the
    /// code → key mapping; the relay does not know what a key means.
    fn lobby_opened(&mut self, _code: &str, _key: Option<&str>) {}
    fn lobby_closed(&mut self, _code: &str) {}
    fn peer_joined(&mut self, _code: &str) {}
    fn peer_left(&mut self, _code: &str) {}

    /// Called on every [`RelayServer::step`], so a policy can refresh its
    /// snapshot or flush a usage batch without owning a thread of its own.
    fn tick(&mut self) {}
}

/// How long a keyed host waits before trying the keyless message as well, in
/// 5 ms polls — see [`RelayHost::host_keyed`]. Long enough that a managed
/// relay has always answered, short enough that an older self-hosted relay
/// still feels instant.
const OLD_RELAY_FALLBACK_POLLS: usize = 300;

/// Lobby codes: 5 characters from an unambiguous alphabet (no 0/O, 1/I).
///
/// A managed relay prefixes its region letter, making the code six — see
/// [`HostAdmission::Allow`]. The prefix is **not** drawn from the alphabet
/// below; it is an operator-allocated letter and the control plane owns the
/// registry.
fn lobby_code(rng: &mut u64, prefix: Option<char>) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    prefix
        .into_iter()
        .chain((0..5)
        .map(|_| {
            let mut x = *rng;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *rng = x;
            ALPHABET[(x >> 33) as usize % ALPHABET.len()] as char
        }))
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
    /// The admission policy, or `None` for the open self-hostable relay —
    /// which is the default, and which behaves exactly as it did before
    /// managed mode existed. Every managed rule lives behind this.
    policy: Option<Box<dyn RelayPolicy>>,
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
        Ok(Self {
            transport,
            conns: HashMap::new(),
            lobbies: HashMap::new(),
            rng: seed,
            port,
            policy: None,
        })
    }

    /// Run under an admission policy — Floptle Cloud's managed mode.
    ///
    /// Without this the relay is the open one ADR-0022 promises: no keys, no
    /// control plane, nothing to authorize against. That is not a fallback, it
    /// is the product — a self-hosted relay must keep working exactly as it
    /// does today, and there is a guard that says so.
    pub fn set_policy(&mut self, policy: Box<dyn RelayPolicy>) {
        self.policy = Some(policy);
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
        // Before the traffic: a managed policy refreshes its key snapshot and
        // flushes its usage batch from here, so it never needs a thread of its
        // own and can never be halfway through a refresh while a host is being
        // admitted.
        if let Some(p) = self.policy.as_mut() {
            p.tick();
        }
        let mut moved = 0;
        for inc in self.transport.poll() {
            moved += 1;
            match inc {
                Incoming::Connected(c) => {
                    self.conns.insert(c, Role::Fresh);
                }
                Incoming::Disconnected(c, _) => self.drop_conn(c),
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
            RelayMsg::Host => self.open_lobby(from, None, None),
            RelayMsg::HostKeyed { key, build } => {
                self.open_lobby(from, Some(&key), build.as_deref())
            }
            RelayMsg::Join { code } => {
                // The cap is enforced here and nowhere else: a live session is
                // never broken for a limit, so the only thing a limit can do is
                // turn away the next arrival — with words that name the plan
                // and where to change it.
                if let Some(p) = self.policy.as_mut()
                    && let JoinAdmission::Refuse { reason } = p.admit_join(&code)
                {
                    self.send(from, Channel::Reliable, &RelayMsg::Refused { reason });
                    return;
                }
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
                self.conns.insert(from, Role::Client { code: code.clone(), game_peer: peer });
                if let Some(p) = self.policy.as_mut() {
                    p.peer_joined(&code);
                }
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

    /// The shared tail of `Host` and `HostKeyed`: ask the policy, then either
    /// open a lobby or say why not.
    ///
    /// **A refusal is a message, never a silence.** The reason goes back on the
    /// wire and reaches the player verbatim — the whole value of "connect your
    /// project at fopull.com/cloud" is that somebody reads it.
    fn open_lobby(&mut self, from: PeerId, key: Option<&str>, build: Option<&str>) {
        let prefix = match self.policy.as_mut() {
            Some(p) => match p.admit_host(key, build) {
                HostAdmission::Allow { prefix } => prefix,
                HostAdmission::Refuse { reason } => {
                    self.send(from, Channel::Reliable, &RelayMsg::Refused { reason });
                    return;
                }
            },
            // No policy: the open relay. A key, if one was presented, is
            // ignored rather than checked — a self-hosted relay has nothing to
            // check it against and is not entitled to an opinion about it.
            None => None,
        };
        let code = loop {
            let c = lobby_code(&mut self.rng, prefix);
            if !self.lobbies.contains_key(&c) {
                break c;
            }
        };
        self.lobbies
            .insert(code.clone(), Lobby { host: from, clients: HashMap::new(), next_peer: 1 });
        self.conns.insert(from, Role::Host { code: code.clone() });
        if let Some(p) = self.policy.as_mut() {
            p.lobby_opened(&code, key);
        }
        self.send(from, Channel::Reliable, &RelayMsg::Hosted { code });
    }

    fn drop_conn(&mut self, c: PeerId) {
        match self.conns.remove(&c) {
            Some(Role::Host { code }) => {
                if let Some(p) = self.policy.as_mut() {
                    p.lobby_closed(&code);
                }
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
                if let Some(p) = self.policy.as_mut() {
                    p.peer_left(&code);
                }
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
    /// Why the relay turned this host away, if it did.
    ///
    /// **This used to be dropped on the floor.** A managed relay's refusals are
    /// the product's own words — "connect your project at fopull.com/cloud",
    /// "this game is at its 20-player limit on the free plan" — and the host
    /// leg parsed `Refused` into `_ => {}`, so all of them arrived as a three
    /// second wait and then "no lobby code (is a relay running there?)": a
    /// message that is not merely unhelpful but points at the wrong thing
    /// entirely, since the relay is plainly running and plainly answering.
    refused: Option<String>,
}

impl RelayHost {
    /// Connect to a relay and host a lobby. Blocks briefly (≤ ~3 s) for the
    /// lobby code — one click, one code.
    pub fn host(relay_addr: &str) -> Result<(Self, String), String> {
        Self::connect_and_host(relay_addr, RelayMsg::Host, None)
    }

    /// Host **as a registered game**, presenting the project's Floptle Cloud
    /// key (and its build hash, when the build has one).
    ///
    /// A managed relay refuses a keyless host, so this is the call an exported
    /// game makes once its project is connected. A self-hosted relay ignores
    /// the key entirely, which is deliberate: it has nothing to check it
    /// against and is not entitled to an opinion about it.
    ///
    /// **The fallback is the compatibility story in the other direction.** New
    /// variants are appended to `RelayMsg`, so a relay older than managed mode
    /// cannot decode `HostKeyed` and drops it — silently, because that is what
    /// an unknown postcard variant does. Rather than let a developer who points
    /// a connected project at their own older relay sit through a three second
    /// timeout and a wrong diagnosis, the plain `Host` goes out after
    /// [`OLD_RELAY_FALLBACK_POLLS`] and that relay hosts them normally. On a
    /// managed relay the answer has always arrived long before then.
    pub fn host_keyed(
        relay_addr: &str,
        key: &str,
        build: Option<&str>,
    ) -> Result<(Self, String), String> {
        Self::connect_and_host(
            relay_addr,
            RelayMsg::HostKeyed { key: key.to_string(), build: build.map(str::to_string) },
            Some(RelayMsg::Host),
        )
    }

    fn connect_and_host(
        relay_addr: &str,
        ask: RelayMsg,
        fallback: Option<RelayMsg>,
    ) -> Result<(Self, String), String> {
        let mut inner = QuicClient::connect(relay_addr)?;
        inner.send(SERVER, Channel::Reliable, &ask.encode());
        let mut me =
            Self { inner, code: None, seq: 0, dedup: SeqState::default(), refused: None };
        let mut fallback = fallback;
        for i in 0..600 {
            let _ = me.poll(); // stashes Hosted{code} / Refused{reason} when it lands
            if let Some(c) = me.code.clone() {
                return Ok((me, c));
            }
            // The relay answered, and the answer was no. Its words, not ours.
            if let Some(reason) = me.refused.take() {
                return Err(reason);
            }
            if i == OLD_RELAY_FALLBACK_POLLS
                && let Some(f) = fallback.take()
            {
                me.inner.send(SERVER, Channel::Reliable, &f.encode());
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
                    // Carried, not swallowed. A managed relay refuses a host
                    // for reasons the player has to be able to act on, and
                    // every one of them is a sentence somebody wrote for them.
                    Some(RelayMsg::Refused { reason }) => {
                        if self.code.is_none() {
                            self.refused = Some(reason.clone());
                        }
                        out.push(Incoming::refused(SERVER, reason));
                    }
                    Some(RelayMsg::PeerJoined { peer }) => out.push(Incoming::Connected(peer)),
                    Some(RelayMsg::PeerLeft { peer }) => out.push(Incoming::dropped(peer)),
                    Some(RelayMsg::FromPeer { peer, channel, seq, bytes })
                        if !self.dedup.stale(peer, channel, seq) =>
                    {
                        out.push(Incoming::Message(peer, tag_channel(channel), bytes));
                    }
                    _ => {}
                },
                Incoming::Disconnected(_, _) => {
                    // The relay leg died: every player is unreachable now, and
                    // the cause is the relay rather than any one of them —
                    // worth saying, because "everyone left at once" is not a
                    // conclusion a game should be left to draw on its own.
                    let peers: Vec<u64> =
                        self.dedup.last.keys().map(|(p, _)| *p).collect();
                    for p in peers {
                        out.push(Incoming::refused(p, "lost the connection to the relay"));
                    }
                }
                Incoming::Connected(_) => {}
            }
        }
        out
    }

    fn stats(&self, _peer: PeerId) -> LinkStats {
        // Only the host↔relay leg is visible from here — a relayed packet's
        // second hop belongs to a connection this endpoint has never seen. The
        // number a game actually wants is host↔player, and it is measured a
        // level up by `NetSession::peer_rtt_ms`, which probes end to end and so
        // works the same over every transport. This stays as the transport's
        // honest answer about the only link it owns.
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
                    // The relay told us exactly what was wrong — usually that
                    // the code doesn't match a lobby. Carry it: mistyping the
                    // code is the most common thing that will ever go wrong in
                    // an online session, and it used to arrive at the game
                    // indistinguishable from the host closing their laptop.
                    Some(RelayMsg::Refused { reason }) => {
                        out.push(Incoming::refused(SERVER, reason));
                    }
                    Some(RelayMsg::FromHost { channel, seq, bytes })
                        if !self.dedup.stale(SERVER, channel, seq) =>
                    {
                        out.push(Incoming::Message(SERVER, tag_channel(channel), bytes));
                    }
                    _ => {}
                },
                // Whatever the leg below knew, if anything.
                Incoming::Disconnected(_, why) => out.push(Incoming::Disconnected(SERVER, why)),
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
    pub(super) struct TestRelay {
        pub(super) port: u16,
        stop: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    /// A policy that answers from a table instead of a control plane — the
    /// managed rules without the network. `floptle-relay`'s real one pulls the
    /// same shape from a key snapshot; what is being asserted here is the
    /// relay's behaviour given an answer, which is the half that lives in this
    /// crate.
    pub(super) struct TablePolicy {
        /// key → ccu_limit. Anything absent is an unknown key.
        keys: HashMap<String, usize>,
        /// Live peers per lobby code, so a cap has something to count.
        live: HashMap<String, usize>,
        /// code → key, because the relay does not know what a key means.
        of_lobby: HashMap<String, String>,
    }

    impl TablePolicy {
        pub(super) fn with(key: &str, limit: usize) -> Self {
            let mut keys = HashMap::new();
            keys.insert(key.to_string(), limit);
            Self { keys, live: HashMap::new(), of_lobby: HashMap::new() }
        }
    }

    impl RelayPolicy for TablePolicy {
        fn admit_host(&mut self, key: Option<&str>, _build: Option<&str>) -> HostAdmission {
            let Some(key) = key.filter(|k| !k.is_empty()) else {
                return HostAdmission::Refuse {
                    reason: "This relay is Floptle Cloud. Connect your project to a game at \
                             fopull.com/cloud, or self-host floptle-relay."
                        .into(),
                };
            };
            match self.keys.contains_key(key) {
                true => HostAdmission::Allow { prefix: Some('U') },
                false => HostAdmission::Refuse {
                    reason: "That game key is not one this relay knows. Check project.ron, \
                             or connect the project at fopull.com/cloud."
                        .into(),
                },
            }
        }

        fn admit_join(&mut self, code: &str) -> JoinAdmission {
            let limit = self
                .of_lobby
                .get(code)
                .and_then(|k| self.keys.get(k))
                .copied()
                .unwrap_or(usize::MAX);
            // The host counts against the cap as well as the clients.
            if self.live.get(code).copied().unwrap_or(0) + 1 >= limit {
                return JoinAdmission::Refuse {
                    reason: format!(
                        "Floptle Cloud: this game is at its {limit}-player limit on the free \
                         plan. Upgrade at fopull.com/cloud."
                    ),
                };
            }
            JoinAdmission::Allow
        }

        fn lobby_opened(&mut self, code: &str, key: Option<&str>) {
            self.live.insert(code.to_string(), 0);
            if let Some(k) = key {
                self.of_lobby.insert(code.to_string(), k.to_string());
            }
        }
        fn lobby_closed(&mut self, code: &str) {
            self.live.remove(code);
            self.of_lobby.remove(code);
        }
        fn peer_joined(&mut self, code: &str) {
            *self.live.entry(code.to_string()).or_insert(0) += 1;
        }
        fn peer_left(&mut self, code: &str) {
            if let Some(n) = self.live.get_mut(code) {
                *n = n.saturating_sub(1);
            }
        }
    }

    impl TestRelay {
        pub(super) fn start() -> Self {
            Self::start_with(None)
        }

        pub(super) fn managed(policy: TablePolicy) -> Self {
            Self::start_with(Some(Box::new(policy)))
        }

        /// `127.0.0.1:<port>`, which is what every endpoint call wants.
        pub(super) fn addr(&self) -> String {
            format!("127.0.0.1:{}", self.port)
        }

        fn start_with(policy: Option<Box<dyn RelayPolicy>>) -> Self {
            let mut relay = RelayServer::bind(0).expect("relay bind");
            if let Some(p) = policy {
                relay.set_policy(p);
            }
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
        let mut server = crate::NetSession::server(Box::new(host_t), 0);
        let mut client = crate::NetSession::client(Box::new(client_t), 0);
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

    /// Rollback inputs must cross a REAL relay in BOTH directions, through the
    /// field's actual sequence: a long menu/lobby phase on the ordinary
    /// predicted path, then the scene switch, then the match.
    ///
    /// Reapplied from the floptle/0039 field report — this was the one
    /// transport no rollback test covered, and the report was right that it
    /// deserved one even though the transport turned out to be innocent. The
    /// bug was above it (`session.rs`'s shared window), which is exactly why a
    /// test that pins the transport's honesty is worth keeping: next time the
    /// symptom looks like this, this test says "not here" in half a second.
    ///
    /// `FLOPTLE_RELAY_ADDR=host:port` points it at a deployed relay instead.
    #[test]
    fn rollback_inputs_cross_a_relay_in_both_directions() {
        use floptle_core::World;
        use crate::{NetInput, NetSession, SERVER};

        let external = std::env::var("FLOPTLE_RELAY_ADDR").ok();
        let _relay = external.is_none().then(TestRelay::start);
        let addr = match (&external, &_relay) {
            (Some(a), _) => a.clone(),
            (None, Some(r)) => format!("127.0.0.1:{}", r.port),
            _ => unreachable!(),
        };
        let held = |a: u64| NetInput { actions: a, ..Default::default() };

        let (host_t, code) = RelayHost::host(&addr).expect("host via relay");
        let client_t = RelayClient::join(&addr, &code).expect("join via relay");
        let mut host = NetSession::server(Box::new(host_t), 0);
        let mut peer = NetSession::client(Box::new(client_t), 0);
        let (hw, mut pw) = (World::default(), World::default());
        host.register_scene(&hw);
        peer.register_scene(&pw);

        // Real UDP needs wall time, not iterations.
        let mut wall = 0u64;
        let pump = |n: u32,
                    wall: &mut u64,
                    host: &mut NetSession,
                    peer: &mut NetSession,
                    pw: &mut World| {
            for _ in 0..n {
                *wall += 1;
                host.tick_server(&hw, *wall);
                peer.tick_client(pw);
                std::thread::sleep(Duration::from_millis(4));
            }
        };
        for _ in 0..100 {
            pump(1, &mut wall, &mut host, &mut peer, &mut pw);
            if peer.is_connected() && peer.my_peer().is_some() {
                break;
            }
        }
        assert!(peer.is_connected(), "the session must handshake through the relay");
        let me = peer.my_peer().expect("the Welcome must assign the joiner a peer id");
        assert_ne!(me, SERVER, "a joiner must not believe it is the host");

        // FIELD SHAPE: the lobby is hosted in the MENU scene, so a long stretch
        // of ordinary predicted traffic — snapshots, acks, pings — runs before
        // the scene switch flips the session into rollback. Anything that
        // survives that transition wrongly only shows up if it happened.
        for _ in 0..60 {
            peer.send_input(wall, held(7));
            pump(1, &mut wall, &mut host, &mut peer, &mut pw);
        }
        host.switch_scene("first");
        host.set_rollback(true, 2, 0x0F0F_16A7_D00D_0001);
        for _ in 0..100 {
            pump(1, &mut wall, &mut host, &mut peer, &mut pw);
            if let Some(s) = peer.take_scene_switch() {
                assert_eq!(s, "first");
                peer.rebind_scene(&pw);
            }
            if peer.take_rollback_start().is_some() {
                break;
            }
        }

        let (mut at_client, mut at_host) =
            (std::collections::HashSet::new(), std::collections::HashSet::new());
        let frontier = |seen: &std::collections::HashSet<u64>| {
            (1..).take_while(|t| seen.contains(t)).last().unwrap_or(0)
        };
        for tick in 1..=24u64 {
            host.push_rollback_input(tick, held(tick));
            peer.send_rollback_input(tick, held(1000 + tick));
            host.set_rollback_confirmed(frontier(&at_host));
            peer.set_rollback_confirmed(frontier(&at_client));
            pump(1, &mut wall, &mut host, &mut peer, &mut pw);
            at_client
                .extend(peer.take_rollback_inputs().iter().filter(|(p, ..)| *p == SERVER).map(
                    |(_, t, _)| *t,
                ));
            at_host.extend(
                host.take_rollback_inputs().iter().filter(|(p, ..)| *p == me).map(|(_, t, _)| *t),
            );
        }
        for _ in 0..60 {
            host.set_rollback_confirmed(frontier(&at_host));
            peer.set_rollback_confirmed(frontier(&at_client));
            pump(1, &mut wall, &mut host, &mut peer, &mut pw);
            at_client
                .extend(peer.take_rollback_inputs().iter().filter(|(p, ..)| *p == SERVER).map(
                    |(_, t, _)| *t,
                ));
            at_host.extend(
                host.take_rollback_inputs().iter().filter(|(p, ..)| *p == me).map(|(_, t, _)| *t),
            );
            if (1..=24).all(|t| at_client.contains(&t) && at_host.contains(&t)) {
                break;
            }
        }
        for tick in 1..=24u64 {
            assert!(at_host.contains(&tick), "HOST never got the client's tick {tick}");
            assert!(
                at_client.contains(&tick),
                "CLIENT never got the host's tick {tick} — the joiner would stall at \
                 warmup+depth with nothing to confirm, which is the shape of the field freeze"
            );
        }
    }

    #[test]
    fn bad_codes_refuse_and_lobbies_die_with_their_host() {
        let relay = TestRelay::start();
        let addr = format!("127.0.0.1:{}", relay.port);

        // Join with a garbage code → refused (a Disconnected on the client).
        let mut nope = RelayClient::join(&addr, "XXXXX").expect("connects to the relay fine");
        // The refusal must arrive WITH the relay's reason. A disconnect that
        // carries nothing is indistinguishable from the host closing their
        // laptop — and mistyping the code is the most common thing that will
        // ever go wrong in an online session, so it is the one failure a game
        // most needs to be able to describe.
        let mut why: Option<Option<String>> = None;
        for _ in 0..400 {
            if let Some(r) = nope.poll().iter().find_map(|i| match i {
                Incoming::Disconnected(SERVER, r) => Some(r.clone()),
                _ => None,
            }) {
                why = Some(r);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let why = why.expect("a bad code must refuse");
        let why = why.expect("the refusal must carry the relay's reason, not just end the link");
        assert!(
            why.contains("XXXXX"),
            "the reason should name the code that failed so a game can print it — got {why:?}"
        );

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
            if client.poll().iter().any(|i| matches!(i, Incoming::Disconnected(SERVER, _))) {
                dead = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(dead, "the lobby must die with its host");
    }
}

/// Managed mode (`floptle/0187`, Floptle Cloud): who may host this relay, who
/// may join, and what a refusal says.
///
/// These drive a **real relay over real QUIC** with a policy that answers from
/// a table, because the thing worth asserting is what an endpoint experiences —
/// a code, or a sentence, or (the bug that started this) three seconds of
/// silence and a wrong diagnosis.
#[cfg(test)]
mod managed_tests {
    use super::tests::*;
    use super::*;

    const KEY: &str = "fk_live_ATESTKEYTHATISNOTREAL0000000";

    /// Pump both ends for a moment and return the refusal the client was given,
    /// if it was given one. The host is polled too, so its own leg keeps
    /// draining and a disturbed session would show up here.
    fn settle(client: &mut RelayClient, host: &mut RelayHost) -> Option<String> {
        let mut why = None;
        for _ in 0..60 {
            for inc in client.poll() {
                if let Incoming::Disconnected(_, Some(reason)) = inc {
                    why = Some(reason);
                }
            }
            let _ = host.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        why
    }

    /// The refusal text, or a failure naming what was expected. `RelayHost` is
    /// not `Debug` (it owns a QUIC endpoint), so `expect_err` is unavailable.
    fn refusal(r: Result<(RelayHost, String), String>, what: &str) -> String {
        match r {
            Err(e) => e,
            Ok((_, code)) => panic!("{what}, but it hosted with code {code}"),
        }
    }

    /// **The negative control, and it is the product.** ADR-0022 promises the
    /// relay stays open and self-hostable: a relay with no policy must behave
    /// exactly as it did before managed mode was written — keyless hosts, five
    /// character codes, nothing to authorize against. If this ever goes red,
    /// managed mode has leaked into the open relay, and that is a licence
    /// question rather than a bug.
    #[test]
    fn a_self_hosted_relay_still_hosts_keyless_with_a_five_character_code() {
        let relay = TestRelay::start();
        let (_t, code) = RelayHost::host(&relay.addr()).expect("a self-hosted relay hosts");
        assert_eq!(code.len(), 5, "the open relay's code is unprefixed: {code}");
    }

    /// A self-hosted relay is handed a key by a Cloud-connected project and
    /// **ignores it** rather than checking it: it has nothing to check against,
    /// and refusing would break a developer's own relay the day they connected
    /// their project to Cloud.
    #[test]
    fn a_self_hosted_relay_ignores_a_key_rather_than_refusing_it() {
        let relay = TestRelay::start();
        let (_t, code) =
            RelayHost::host_keyed(&relay.addr(), KEY, None).expect("keys are not its business");
        assert_eq!(code.len(), 5, "still the open relay's own code: {code}");
    }

    /// Ty's rule, path 1: **a game with no key cannot use a managed relay** —
    /// and is told where to go rather than left guessing.
    #[test]
    fn a_managed_relay_refuses_a_keyless_host_and_says_where_to_go() {
        let relay = TestRelay::managed(TablePolicy::with(KEY, 20));
        let err = refusal(RelayHost::host(&relay.addr()), "a keyless host is refused");
        assert!(err.contains("fopull.com/cloud"), "the refusal must name the fix: {err}");
        assert!(
            err.contains("self-host floptle-relay"),
            "…and the other way out, because it is a real one: {err}"
        );
    }

    /// Ty's rule, path 2. An unknown key gets a **different** sentence to a
    /// missing one: a developer who has not connected their project yet and a
    /// developer whose key was revoked need different next actions, and one
    /// message for both sends the first one hunting for a problem they do not
    /// have.
    #[test]
    fn an_unknown_key_is_refused_with_different_words_to_a_missing_one() {
        let relay = TestRelay::managed(TablePolicy::with(KEY, 20));
        let missing = refusal(RelayHost::host(&relay.addr()), "keyless is refused");
        let unknown = refusal(
            RelayHost::host_keyed(&relay.addr(), "fk_live_NOPE", None),
            "an unknown key is refused",
        );
        assert!(unknown.contains("project.ron"), "point at where the key lives: {unknown}");
        assert_ne!(missing, unknown, "two different problems must not read identically");
    }

    /// **The refusal arrives as a refusal.** This is the bug the managed work
    /// found in the existing code: the host leg parsed `Refused` into `_ => {}`,
    /// so every one of these sentences was dropped and the caller waited three
    /// seconds and then reported "no lobby code (is a relay running there?)" —
    /// which is both unhelpful and false, since the relay is plainly running
    /// and plainly answering. Watched failing.
    #[test]
    fn a_refusal_reaches_the_host_instead_of_a_timeout_that_blames_the_relay() {
        let relay = TestRelay::managed(TablePolicy::with(KEY, 20));
        let t0 = std::time::Instant::now();
        let err = refusal(RelayHost::host(&relay.addr()), "refused");
        assert!(
            !err.contains("is a relay running there"),
            "the timeout message means the answer was thrown away: {err}"
        );
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "it answered immediately; waiting {:?} means we polled past the answer",
            t0.elapsed()
        );
    }

    /// A good key hosts, and the code carries the **region letter** so a client
    /// can map it back to a relay without asking anybody — which is what keeps
    /// the join path independent of the control plane.
    #[test]
    fn a_good_key_hosts_and_the_code_carries_its_region() {
        let relay = TestRelay::managed(TablePolicy::with(KEY, 20));
        let (_t, code) = RelayHost::host_keyed(&relay.addr(), KEY, None).expect("hosts");
        assert_eq!(code.len(), 6, "region letter + five: {code}");
        assert!(code.starts_with('U'), "us-east is U: {code}");
    }

    /// The CCU cap turns away **the next arrival**, and the sentence names the
    /// plan and where to change it. A cap that dropped a live player would be a
    /// worse product than no cap at all.
    #[test]
    fn the_cap_refuses_the_next_joiner_and_never_the_live_session() {
        // Two: the host, and one client. The second client is over.
        let relay = TestRelay::managed(TablePolicy::with(KEY, 2));
        let (mut host, code) = RelayHost::host_keyed(&relay.addr(), KEY, None).expect("hosts");
        let mut first = RelayClient::join(&relay.addr(), &code).expect("connects");
        // `join` is non-blocking by design — the verdict arrives on `poll`,
        // which is how a real client learns it too.
        let admitted = settle(&mut first, &mut host);
        assert_eq!(admitted, None, "the first joiner is under the cap: {admitted:?}");

        let mut second = RelayClient::join(&relay.addr(), &code).expect("connects");
        let msg = settle(&mut second, &mut host)
            .expect("the 2nd client was admitted over a 2-player cap");
        assert!(msg.contains("limit"), "say what happened: {msg}");
        assert!(msg.contains("fopull.com/cloud"), "…and where to fix it: {msg}");

        // **The live session is untouched.** Nobody was dropped to make room —
        // a cap that evicted a player mid-game would be a worse product than
        // no cap at all.
        assert_eq!(settle(&mut first, &mut host), None, "the seated player was disturbed");
    }
}
