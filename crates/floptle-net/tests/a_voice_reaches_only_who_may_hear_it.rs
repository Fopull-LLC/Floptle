//! floptle/0180 — voice forwarding, and the range gating that makes proximity
//! voice a real thing rather than a volume slider.
//!
//! The card was blunt about why this lives on the server: "attenuating a stream
//! every client already received is not proximity voice, it is a volume slider
//! a modified client turns back up — and in a hidden-role game hearing someone
//! is knowing where they are."
//!
//! So these assert on the only thing that settles it: whether the BYTES leave
//! the server. A test that checked playback volume would pass just as happily
//! with the leak in place.

use floptle_core::World;
use floptle_net::{MemoryHub, NetSession};

/// A host and three clients, connected and past the handshake.
struct Lobby {
    server: NetSession,
    world: World,
    clients: Vec<(NetSession, World)>,
}

fn lobby(n: usize) -> Lobby {
    let hub = MemoryHub::new();
    let mut server = NetSession::server(Box::new(hub.server_endpoint()), 0);
    let world = World::default();
    let mut clients: Vec<(NetSession, World)> = (0..n)
        .map(|_| (NetSession::client(Box::new(hub.connect()), 0), World::default()))
        .collect();
    for t in 1..4 {
        server.tick_server(&world, t);
        for (c, cw) in clients.iter_mut() {
            c.tick_client(cw);
        }
    }
    assert_eq!(server.peers().len(), n, "everyone joined");
    Lobby { server, world, clients }
}

impl Lobby {
    /// Pump one tick and collect what each client heard, by speaker.
    fn pump(&mut self) -> Vec<Vec<u64>> {
        self.server.tick_server(&self.world, 10);
        self.clients
            .iter_mut()
            .map(|(c, cw)| {
                c.tick_client(cw);
                c.take_voice().into_iter().map(|(speaker, _, _)| speaker).collect()
            })
            .collect()
    }
}

/// A frame is not real audio here — the codec is tested in `floptle-audio`.
/// What matters on this side is who receives the bytes.
const FRAME: &[u8] = b"an encoded 20 ms of speech";

#[test]
fn by_default_everyone_hears_a_speaker_except_the_speaker() {
    let mut l = lobby(3);
    let speaker = l.server.peers()[0];
    l.clients[0].0.send_voice(FRAME);
    l.server.tick_server(&l.world, 5);
    let heard = l.pump();

    assert!(heard[0].is_empty(), "a speaker never gets their own voice back");
    assert_eq!(heard[1], vec![speaker], "…and everyone else does, stamped with who said it");
    assert_eq!(heard[2], vec![speaker]);
}

/// THE one. Peer 3 is out of earshot, so peer 3 is never sent the bytes — not
/// sent them quietly.
#[test]
fn a_peer_out_of_range_is_never_sent_the_audio_at_all() {
    let mut l = lobby(3);
    let peers = l.server.peers().to_vec();
    let (speaker, near) = (peers[0], peers[1]);
    l.server.set_voice_forward(speaker, vec![near]);

    l.clients[0].0.send_voice(FRAME);
    l.server.tick_server(&l.world, 5);
    let heard = l.pump();

    assert_eq!(heard[1], vec![speaker], "the player standing next to them hears it");
    assert!(
        heard[2].is_empty(),
        "the player across the house received NOTHING — not a quiet copy of it"
    );
}

/// Proximity voice re-decides every tick or so as people move. Both directions
/// have to work, or someone walks out of earshot and is never heard again.
#[test]
fn the_gate_reopens_when_the_speaker_comes_back_into_range() {
    let mut l = lobby(2);
    let peers = l.server.peers().to_vec();
    let speaker = peers[0];

    l.server.set_voice_forward(speaker, vec![]); // nobody is near
    l.clients[0].0.send_voice(FRAME);
    l.server.tick_server(&l.world, 5);
    assert!(l.pump()[1].is_empty(), "out of earshot");

    l.server.clear_voice_forward(speaker); // they walked back over
    l.clients[0].0.send_voice(FRAME);
    l.server.tick_server(&l.world, 6);
    assert_eq!(l.pump()[1], vec![speaker], "and can be heard again");
}

/// The "dead can only talk to the dead" channel, which is the same mechanism
/// pointed at a game rule rather than at a distance.
#[test]
fn a_game_can_route_a_speaker_to_an_arbitrary_set() {
    let mut l = lobby(3);
    let peers = l.server.peers().to_vec();
    l.server.set_voice_forward(peers[0], vec![peers[2]]);
    l.clients[0].0.send_voice(FRAME);
    l.server.tick_server(&l.world, 5);
    let heard = l.pump();
    assert!(heard[1].is_empty(), "the living hear nothing");
    assert_eq!(heard[2], vec![peers[0]], "the dead hear each other");
}

/// A client cannot put words in another player's mouth: the SERVER stamps the
/// speaker. In a hidden-role game that is not a prank, it is a win condition.
#[test]
fn the_speaker_is_stamped_by_the_server_not_claimed_by_the_client() {
    let mut l = lobby(2);
    let peers = l.server.peers().to_vec();
    l.clients[1].0.send_voice(FRAME);
    l.server.tick_server(&l.world, 5);
    let heard = l.pump();
    assert_eq!(
        heard[0],
        vec![peers[1]],
        "attributed to whoever actually sent it, on the server's own authority"
    );
}

/// A kicked or departed peer stops being heard immediately — otherwise a
/// removed player keeps talking to the lobby.
#[test]
fn a_peer_that_is_off_the_roster_is_not_forwarded() {
    let mut l = lobby(2);
    let peers = l.server.peers().to_vec();
    l.server.kick(peers[0], "griefing");
    l.clients[0].0.send_voice(FRAME);
    l.server.tick_server(&l.world, 5);
    let heard = l.pump();
    assert!(heard[1].is_empty(), "a removed player does not keep talking to the lobby");
}

/// Voice must not disturb the rest of the session: it is unreliable, and it
/// never touches the snapshot or the tick.
#[test]
fn voice_does_not_interfere_with_the_session() {
    let mut l = lobby(2);
    for _ in 0..50 {
        l.clients[0].0.send_voice(FRAME);
        l.server.tick_server(&l.world, 5);
        l.pump();
    }
    assert_eq!(l.server.peers().len(), 2, "everyone is still connected");
    assert!(l.clients[0].0.is_connected());
    assert!(l.clients[1].0.is_connected());
}

/// Forwarding rules are per-peer state and must not outlive the peer, or a
/// later joiner inherits a stranger's earshot.
#[test]
fn a_departed_peers_routing_is_forgotten() {
    let mut l = lobby(2);
    let peers = l.server.peers().to_vec();
    l.server.set_voice_forward(peers[0], vec![peers[1]]);
    assert!(l.server.voice_forward(peers[0]).is_some());
    l.server.kick(peers[0], "bye");
    assert!(l.server.voice_forward(peers[0]).is_none(), "the rule left with them");
    assert_eq!(
        l.server.voice_forward(peers[1]),
        None,
        "and nobody else inherited a reference to them"
    );
}

/// A dedicated server forwards voice and listens to none of it — no output
/// device, nobody sitting there. Its own copy of every frame must not pile up:
/// unbounded, this is a slow leak on the one machine expected to stay up for
/// weeks, and it would only ever show up in production.
#[test]
fn a_server_that_never_listens_does_not_accumulate_voice_forever() {
    let mut l = lobby(2);
    for tick in 0..4_000u64 {
        l.clients[0].0.send_voice(FRAME);
        l.server.tick_server(&l.world, 5 + tick);
        // Deliberately never `take_voice()` on the server — that is exactly
        // what `floptle-runtime --server` looked like before it drained.
    }
    let held = l.server.take_voice().len();
    assert!(
        held <= 8 * 50,
        "the server is holding {held} undrained voice frames — that is a leak"
    );
}
