//! floptle/0180 — the whole voice path, end to end, on one machine.
//!
//! Every other test in this feature checks one link: the codec round-trips, the
//! jitter buffer reorders, the server forwards to the right peers, a stream
//! plays as a spatial voice. This one runs the actual chain a spoken word takes
//! and listens to what comes out the far end:
//!
//! ```text
//! speech samples → VoiceEncoder → NetSession::send_voice → the server forwards
//!   → the listener's take_voice → VoiceJitter → StreamRing → AudioCore
//!   → rendered stereo
//! ```
//!
//! That matters more than the sum of the parts. Each link was written against
//! an idea of what its neighbour wanted; only running them together shows
//! whether they agreed — and the failure this catches is the whole feature
//! being silent for a reason no single unit test can see.

use floptle_audio::chat::{VoiceEncoder, VoiceJitter, FRAME_SAMPLES};
use floptle_audio::stream::StreamRing;
use floptle_audio::voice::AudioCore;
use floptle_audio::{PlayParams, SpatialMode};
use floptle_core::math::DVec3;
use floptle_core::World;
use floptle_net::{MemoryHub, NetSession};

/// A second of crude speech: a glottal buzz with formants, amplitude-modulated
/// like syllables. Silence would be correctly discarded by the encoder's DTX,
/// which would make this test pass by testing nothing.
fn speech(frames: usize) -> Vec<f32> {
    let n = frames * FRAME_SAMPLES;
    (0..n)
        .map(|i| {
            let t = i as f32 / 48_000.0;
            let syllable = 0.5 + 0.5 * (std::f32::consts::TAU * 4.0 * t).sin();
            ((std::f32::consts::TAU * 130.0 * t).sin()
                + 0.5 * (std::f32::consts::TAU * 700.0 * t).sin()
                + 0.25 * (std::f32::consts::TAU * 1220.0 * t).sin())
                * 0.25
                * syllable
        })
        .collect()
}

fn peak(s: &[f32]) -> f32 {
    s.iter().map(|v| v.abs()).fold(0.0f32, f32::max)
}

/// A host and two clients, past the handshake.
struct Session {
    server: NetSession,
    world: World,
    clients: Vec<(NetSession, World)>,
}

fn session(n: usize) -> Session {
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
    Session { server, world, clients }
}

/// Run the real loop: peer 0 speaks, the server forwards, peer 1 listens and
/// its audio thread renders. Returns what peer 1 actually heard.
fn converse(s: &mut Session, pcm: &[f32], listener_at: DVec3, speaker_at: DVec3) -> Vec<f32> {
    let mut enc = VoiceEncoder::new().expect("encoder");
    let mut jitter = VoiceJitter::new().expect("jitter");
    let ring = StreamRing::new(48_000);

    let mut core = AudioCore::new(48_000.0, 128);
    core.set_listener(floptle_audio::Listener { position: listener_at, ..Default::default() });
    core.play_stream(
        1,
        std::sync::Arc::clone(&ring),
        Some(speaker_at),
        PlayParams {
            mode: SpatialMode::Spatial,
            min_distance: 2.0,
            max_distance: 30.0,
            ..Default::default()
        },
    );

    let mut heard = Vec::new();
    let (mut l, mut r) = (vec![0.0f32; FRAME_SAMPLES], vec![0.0f32; FRAME_SAMPLES]);
    for (tick, frame) in pcm.as_chunks::<FRAME_SAMPLES>().0.iter().enumerate() {
        // …speak.
        if let Some(packet) = enc.encode(frame).expect("encode") {
            s.clients[0].0.send_voice(packet);
        }
        // …the server forwards, the listener receives.
        s.server.tick_server(&s.world, 10 + tick as u64);
        let (listener, lworld) = &mut s.clients[1];
        listener.tick_client(lworld);
        for (_speaker, seq, payload) in listener.take_voice() {
            jitter.accept(seq, &payload);
        }
        // …and the listener's audio thread plays whatever is due.
        jitter.drain_into(&ring);
        l.fill(0.0);
        r.fill(0.0);
        core.render(&mut l, &mut r);
        // BOTH ears. A speaker off to one side pans almost entirely into one
        // channel, so collecting the left alone would call a perfectly audible
        // voice silent.
        heard.extend(l.iter().zip(&r).map(|(a, b)| a.abs().max(b.abs())));
    }
    heard
}

/// The one that matters: somebody speaks, and somebody else hears it.
#[test]
fn a_word_spoken_on_one_client_is_heard_on_another() {
    let mut s = session(2);
    let heard = converse(&mut s, &speech(40), DVec3::ZERO, DVec3::new(3.0, 0.0, 0.0));
    assert!(
        peak(&heard) > 0.02,
        "the listener heard nothing at all — peak {:.4}",
        peak(&heard)
    );
    // Not one lucky click: a real fraction of the conversation carried.
    let loud = heard.iter().filter(|v| v.abs() > 0.01).count();
    assert!(
        loud > heard.len() / 20,
        "only {loud} of {} samples carried audio",
        heard.len()
    );
}

/// Proximity voice is the whole point: the same words, further away, are
/// quieter. This is the property a game builds "who can hear you, and from
/// where" on top of.
#[test]
fn the_same_words_are_quieter_from_across_the_room() {
    let pcm = speech(40);
    let near = {
        let mut s = session(2);
        peak(&converse(&mut s, &pcm, DVec3::ZERO, DVec3::new(2.0, 0.0, 0.0)))
    };
    let far = {
        let mut s = session(2);
        peak(&converse(&mut s, &pcm, DVec3::ZERO, DVec3::new(25.0, 0.0, 0.0)))
    };
    assert!(near > 0.02, "the near speaker was audible: {near:.4}");
    assert!(
        far < near * 0.5,
        "distance must attenuate: near {near:.4} vs far {far:.4}"
    );
}

/// The server-side gate, proven at the far end rather than at the wire: a peer
/// the game has not named hears actual silence, not a quiet copy.
#[test]
fn a_peer_the_server_did_not_forward_to_hears_silence() {
    let mut s = session(2);
    let listener = s.server.peers()[1];
    let speaker = s.server.peers()[0];
    // Nobody may hear this speaker.
    s.server.set_voice_forward(speaker, vec![]);
    let heard = converse(&mut s, &speech(40), DVec3::ZERO, DVec3::new(3.0, 0.0, 0.0));
    assert_eq!(peak(&heard), 0.0, "not attenuated — never sent");
    let _ = listener;
}

/// One in five datagrams lost, which is a far worse link than anything real.
/// The conversation has to survive it — degraded, but a conversation.
#[test]
fn a_lossy_link_degrades_the_voice_instead_of_ending_it() {
    let mut s = session(2);
    let mut enc = VoiceEncoder::new().unwrap();
    let mut jitter = VoiceJitter::new().unwrap();
    let ring = StreamRing::new(48_000);
    let mut core = AudioCore::new(48_000.0, 128);
    core.play_stream(1, std::sync::Arc::clone(&ring), None, PlayParams::default());

    let pcm = speech(60);
    let mut heard = Vec::new();
    let (mut l, mut r) = (vec![0.0f32; FRAME_SAMPLES], vec![0.0f32; FRAME_SAMPLES]);
    for (i, frame) in pcm.as_chunks::<FRAME_SAMPLES>().0.iter().enumerate() {
        if let Some(p) = enc.encode(frame).unwrap() {
            s.clients[0].0.send_voice(p);
        }
        s.server.tick_server(&s.world, 10 + i as u64);
        let (listener, lworld) = &mut s.clients[1];
        listener.tick_client(lworld);
        for (_, seq, payload) in listener.take_voice() {
            if i % 5 == 3 {
                continue; // this datagram never made it
            }
            jitter.accept(seq, &payload);
        }
        jitter.drain_into(&ring);
        l.fill(0.0);
        r.fill(0.0);
        core.render(&mut l, &mut r);
        heard.extend(l.iter().zip(&r).map(|(a, b)| a.abs().max(b.abs())));
    }
    assert!(peak(&heard) > 0.02, "20% loss must not silence the speaker");
    assert!(jitter.concealed() > 0, "and the loss was concealed, not waited on");
    assert_eq!(core.active_voices(), 1, "the voice is still live at the end");
}
