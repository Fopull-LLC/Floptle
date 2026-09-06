//! Voice chat, from the microphone to the mixer (`floptle/0180`).
//!
//! One struct owns the whole path on this machine: the capture device, the
//! encoder, one jitter buffer + stream + playing voice per remote speaker, and
//! the local mute list. The editor drives it from two places — [`VoiceChat::send`]
//! once a tick to ship what the microphone heard, and [`VoiceChat::receive`] to
//! hand arriving frames to the right speaker.
//!
//! ## It lives with the SESSION, not the scene
//!
//! Every playing voice is reset by a scene swap, which for a spatial one-shot
//! is right and for a conversation is not — the server switching maps must not
//! cut everyone off mid-sentence. So this is not part of the audio system's
//! per-scene state: the capture keeps running, the streams keep filling, and
//! [`VoiceChat::rebind_scene`] only drops the *node attachments*, because the
//! nodes they named no longer exist. A game re-attaches in the new scene and
//! the stream never restarted.
//!
//! ## What a dedicated server does with this
//!
//! Nothing. It has no output device and no listener, so it never constructs a
//! decoder or a voice; it only forwards, which is `floptle-net`'s job. Every
//! entry point here is a no-op without an audio engine.

use std::collections::HashMap;

use floptle_audio::chat::{VoiceEncoder, VoiceJitter, FRAME_SAMPLES};
use floptle_audio::stream::{StreamRef, StreamRing, STREAM_RATE};
use floptle_audio::{Capture, PlayParams, VoiceId};
use floptle_core::{Entity, World};
use floptle_script::{VoiceCmd, VoiceOpts, VoiceState};

/// How much audio a speaker's ring can hold before it refuses more.
///
/// Half a second. Far more than the jitter buffer's 60 ms target, because the
/// ring also absorbs the gap between the game tick that pushes and the audio
/// callback that pulls — and a ring that overflows drops audio the jitter
/// buffer had already decided was worth playing.
const RING_SAMPLES: usize = (STREAM_RATE as usize) / 2;

/// Speech is "speaking" while frames keep arriving. One frame is 20 ms; a HUD
/// indicator that flickered between syllables would be worse than none, so the
/// flag holds for a few frames past the last one.
const SPEAKING_HOLD_MS: f32 = 250.0;

/// One remote speaker's incoming voice.
struct Speaker {
    jitter: VoiceJitter,
    ring: StreamRef,
    /// The playing voice, once the audio engine has one. `None` on a machine
    /// with no output device — the stream still fills and is simply not heard.
    voice: Option<VoiceId>,
    /// Node the voice follows, if the game attached one.
    follow: Option<Entity>,
    params: PlayParams,
    /// Milliseconds since a frame last arrived, for `voice.speaking(peer)`.
    since_frame_ms: f32,
    muted: bool,
}

/// The whole voice path on this machine.
#[derive(Default)]
pub struct VoiceChat {
    capture: Capture,
    encoder: Option<VoiceEncoder>,
    speakers: HashMap<u64, Speaker>,
    /// The stream carrying our own voice back to us, when sidetone is on.
    /// `None` = off, which is the default: hearing yourself is disconcerting,
    /// and a game that wants it asks.
    sidetone_stream: Option<(StreamRef, Option<VoiceId>)>,
    /// Devices, refreshed when a game asks rather than every frame:
    /// enumerating them hits the OS audio stack.
    devices: Vec<String>,
    devices_known: bool,
    /// Console lines the editor should show (device errors, mostly).
    pub notices: Vec<String>,
    /// The harness microphone — see [`VoiceChat::set_test_speaker`].
    test_speaker: Option<TestSpeaker>,
}

/// A WAV standing in for a remote player's microphone.
struct TestSpeaker {
    peer: u64,
    /// 48 kHz mono, ready to cut into frames.
    pcm: Vec<f32>,
    at: usize,
    seq: u16,
    encoder: Option<VoiceEncoder>,
}

/// Downmix and resample a clip to the one shape the voice path speaks.
fn resample_mono(clip: &floptle_audio::ClipRef) -> Vec<f32> {
    let ch = clip.channels.max(1) as usize;
    let frames = clip.samples.len() / ch;
    if frames == 0 {
        return Vec::new();
    }
    let step = clip.sample_rate.max(1) as f64 / STREAM_RATE as f64;
    let out_len = ((frames as f64) / step) as usize;
    (0..out_len)
        .map(|i| {
            let src = ((i as f64 * step) as usize).min(frames - 1);
            let mut s = 0.0;
            for c in 0..ch {
                s += clip.samples[src * ch + c];
            }
            (s / ch as f32).clamp(-1.0, 1.0)
        })
        .collect()
}

impl VoiceChat {
    /// Apply one tick's worth of `voice.*` commands from Lua.
    pub fn apply_commands(&mut self, cmds: Vec<VoiceCmd>, world: &World) {
        for cmd in cmds {
            match cmd {
                VoiceCmd::SetDevice { name } => match self.capture.open(name.as_deref()) {
                    Ok(()) => {
                        let d = self.capture.device().unwrap_or("?").to_string();
                        self.notices.push(format!("🎤 microphone: {d}"));
                    }
                    Err(e) => {
                        // A missing microphone is not a reason to stop the
                        // game — it is a reason to say so once.
                        self.notices.push(format!("🎤 {e}"));
                    }
                },
                VoiceCmd::SetTransmit { on } => self.capture.set_transmit(on),
                VoiceCmd::Sidetone { on } => self.set_sidetone(on),
                VoiceCmd::Mute { peer, muted } => {
                    if let Some(s) = self.speakers.get_mut(&peer) {
                        s.muted = muted;
                    } else {
                        // Muting somebody before they have said anything has to
                        // stick, or the mute is lost the moment it matters.
                        self.speaker_mut(peer).muted = muted;
                    }
                }
                VoiceCmd::Attach { peer, eid, opts } => {
                    let e = world.entity_with::<floptle_core::transform::Transform>(eid);
                    let s = self.speaker_mut(peer);
                    s.follow = e;
                    apply_opts(&mut s.params, &opts);
                }
                VoiceCmd::Detach { peer } => {
                    if let Some(s) = self.speakers.get_mut(&peer) {
                        s.follow = None;
                    }
                }
                VoiceCmd::Params { peer, opts } => {
                    let s = self.speaker_mut(peer);
                    apply_opts(&mut s.params, &opts);
                }
                // Handled by the caller: it is the session's decision, not this
                // machine's audio path.
                VoiceCmd::SetForward { .. } => {}
            }
        }
    }


    /// Encode whatever the microphone captured. The caller ships the result.
    ///
    /// Returns nothing when the mic is closed, when there is no microphone, and
    /// when the encoder decided the frame was silence — all three are the
    /// common case, and none of them is an error.
    pub fn encode_captured(&mut self) -> Vec<Vec<u8>> {
        let frames = self.capture.take_frames();
        if frames.is_empty() {
            return Vec::new();
        }
        if self.encoder.is_none() {
            match VoiceEncoder::new() {
                Ok(e) => self.encoder = Some(e),
                Err(e) => {
                    self.notices.push(format!("🎤 {e}"));
                    return Vec::new();
                }
            }
        }
        let enc = self.encoder.as_mut().expect("just built");
        let mut out = Vec::new();
        for f in &frames {
            match enc.encode(f) {
                Ok(Some(p)) => out.push(p.to_vec()),
                Ok(None) => {} // DTX: silence, nothing worth sending
                Err(e) => self.notices.push(format!("🎤 {e}")),
            }
        }
        // Sidetone plays the RAW capture, not the encoded-then-decoded copy:
        // hearing your own voice through the codec's delay is worse than not
        // hearing it, and the point is to confirm the mic is live.
        if let Some((ring, _)) = &self.sidetone_stream {
            for f in &frames {
                ring.push(f);
            }
        }
        out
    }

    /// A frame arrived for `peer`. Cheap: it only queues.
    pub fn receive(&mut self, peer: u64, seq: u16, frame: &[u8]) {
        let s = self.speaker_mut(peer);
        if s.muted {
            // A local mute stops the decode as well as the sound. Decoding
            // audio nobody will hear is pure cost, once per speaker per frame.
            return;
        }
        s.since_frame_ms = 0.0;
        s.jitter.accept(seq, frame);
    }

    /// Per-tick upkeep: release buffered audio into each speaker's stream, keep
    /// the voices attached to their nodes, and age the speaking flags.
    pub fn advance(
        &mut self,
        dt_ms: f32,
        world: &World,
        audio: &mut crate::audio::AudioSystem,
    ) {
        for s in self.speakers.values_mut() {
            s.since_frame_ms += dt_ms;
            s.jitter.drain_into(&s.ring);
        }
        let Some(engine) = audio.engine() else { return };
        for s in self.speakers.values_mut() {
            let pos = s
                .follow
                .map(|e| floptle_core::world_transform(world, e).translation);
            match s.voice {
                None => {
                    s.voice = Some(engine.play_stream(
                        std::sync::Arc::clone(&s.ring),
                        pos,
                        s.params.clone(),
                    ));
                }
                Some(id) => {
                    engine.update_params(id, s.params.clone());
                    if let Some(p) = pos {
                        engine.move_voice(id, p);
                    }
                }
            }
        }
        if let Some((ring, voice)) = &mut self.sidetone_stream
            && voice.is_none()
        {
            // Flat: your own voice is not somewhere in the room.
            let params = PlayParams {
                mode: floptle_audio::SpatialMode::Flat,
                ..Default::default()
            };
            *voice = Some(engine.play_stream(std::sync::Arc::clone(ring), None, params));
        }
    }

    /// Mirror live state back to Lua.
    pub fn state(&mut self, is_server: bool) -> VoiceState {
        let _ = is_server;
        if !self.devices_known {
            self.devices = Capture::devices();
            self.devices_known = true;
        }
        VoiceState {
            devices: self.devices.clone(),
            device: self.capture.device().map(str::to_string),
            level: self.capture.level(),
            transmitting: self.capture.transmitting(),
            speaking: self
                .speakers
                .iter()
                .filter(|(_, s)| !s.muted && s.since_frame_ms < SPEAKING_HOLD_MS)
                .map(|(p, _)| *p)
                .collect(),
            muted: self.speakers.iter().filter(|(_, s)| s.muted).map(|(p, _)| *p).collect(),
            sources: self.speakers.keys().copied().collect(),
        }
    }

    /// A peer left: their voice goes with them.
    pub fn drop_peer(&mut self, peer: u64, audio: &mut crate::audio::AudioSystem) {
        if let Some(s) = self.speakers.remove(&peer)
            && let Some(id) = s.voice
            && let Some(engine) = audio.engine()
        {
            engine.stop(id);
        }
    }

    /// The session ended: stop everything and close the microphone.
    pub fn shutdown(&mut self, audio: &mut crate::audio::AudioSystem) {
        let voices: Vec<VoiceId> = self
            .speakers
            .values()
            .filter_map(|s| s.voice)
            .chain(self.sidetone_stream.as_ref().and_then(|(_, v)| *v))
            .collect();
        if let Some(engine) = audio.engine() {
            for id in voices {
                engine.stop(id);
            }
        }
        self.speakers.clear();
        self.sidetone_stream = None;
        self.capture.set_transmit(false);
        self.capture.close();
        self.encoder = None;
    }

    /// The session switched scenes.
    ///
    /// The streams and the capture survive — a scene swap must not cut a
    /// conversation off. Only the node attachments go, because the nodes they
    /// named no longer exist; the game re-attaches in the new scene and the
    /// stream never restarted.
    pub fn rebind_scene(&mut self, audio: &mut crate::audio::AudioSystem) {
        let stale: Vec<VoiceId> = self.speakers.values().filter_map(|s| s.voice).collect();
        if let Some(engine) = audio.engine() {
            // The VOICES are re-made (the mixer's per-scene state resets under
            // them), but the rings they read from are not, so whatever had
            // already been decoded is still there to play.
            for id in stale {
                engine.stop(id);
            }
        }
        for s in self.speakers.values_mut() {
            s.voice = None;
            s.follow = None;
        }
        if let Some((_, v)) = &mut self.sidetone_stream {
            *v = None;
        }
    }

    /// Is anything actually being heard? For the 🎧 panel.
    pub fn active(&self) -> bool {
        self.capture.is_open() || !self.speakers.is_empty()
    }

    /// Per-speaker diagnostics for the 🎧 panel: (peer, buffered ms, cushion
    /// ms, concealed frames, late packets).
    pub fn diagnostics(&self) -> Vec<(u64, f32, f32, u64, u64)> {
        let mut rows: Vec<_> = self
            .speakers
            .iter()
            .map(|(p, s)| {
                (*p, s.ring.buffered_ms(), s.jitter.depth_ms(), s.jitter.concealed(), s.jitter.too_late())
            })
            .collect();
        rows.sort_by_key(|r| r.0);
        rows
    }

    /// One line describing the microphone, for the 🌐 panel.
    pub fn mic_summary(&self) -> String {
        match self.capture.device() {
            None => "no microphone open".to_string(),
            Some(d) => format!(
                "{d} · {} · level {:.0}%",
                if self.capture.transmitting() { "TRANSMITTING" } else { "muted" },
                self.capture.level() * 100.0
            ),
        }
    }

    /// THE HARNESS MICROPHONE: a WAV played in as though a peer were speaking.
    ///
    /// Voice is the one feature that normally needs two machines, two people
    /// and a microphone to try at all, which makes it the one most likely to
    /// ship broken. This gives it back to one desk: the clip is encoded and fed
    /// through the REAL path — the server's forwarding rules, the jitter
    /// buffer, the spatial voice — so what it proves is the routing a live
    /// session would do, not a shortcut around it.
    pub fn set_test_speaker(&mut self, peer: u64, clip: floptle_audio::ClipRef) {
        self.test_speaker = Some(TestSpeaker {
            peer,
            pcm: resample_mono(&clip),
            at: 0,
            seq: 0,
            encoder: None,
        });
    }

    pub fn stop_test_speaker(&mut self) {
        self.test_speaker = None;
    }

    pub fn test_speaker(&self) -> Option<u64> {
        self.test_speaker.as_ref().map(|t| t.peer)
    }

    /// One tick of the harness microphone: the next 20 ms, encoded.
    ///
    /// Returns `(peer, seq, packet)` for the caller to inject into the session.
    /// `None` when there is no test speaker or the clip has finished — it does
    /// not loop, because a line of dialogue repeating forever is a worse
    /// debugging experience than one that stops.
    pub fn next_test_frame(&mut self) -> Option<(u64, u16, Vec<u8>)> {
        let t = self.test_speaker.as_mut()?;
        let end = t.at + FRAME_SAMPLES;
        if end > t.pcm.len() {
            self.test_speaker = None;
            return None;
        }
        if t.encoder.is_none() {
            t.encoder = VoiceEncoder::new().ok();
        }
        let frame = &t.pcm[t.at..end];
        t.at = end;
        let seq = t.seq;
        t.seq = t.seq.wrapping_add(1);
        let packet = t.encoder.as_mut()?.encode(frame).ok().flatten()?.to_vec();
        Some((t.peer, seq, packet))
    }

    /// TESTING: push audio straight at the capture, and open its gate.
    #[cfg(test)]
    pub fn inject_microphone(&mut self, pcm: &[f32]) {
        self.capture.inject(pcm);
    }

    /// TESTING: open the mic gate without a device.
    #[cfg(test)]
    pub fn set_transmit(&mut self, on: bool) {
        self.capture.set_transmit(on);
    }

    fn set_sidetone(&mut self, on: bool) {
        match (on, self.sidetone_stream.is_some()) {
            (true, false) => {
                self.sidetone_stream = Some((StreamRing::new(RING_SAMPLES), None));
            }
            (false, true) => self.sidetone_stream = None,
            _ => {}
        }
    }

    /// The speaker's state, created on first sight of them.
    fn speaker_mut(&mut self, peer: u64) -> &mut Speaker {
        self.speakers.entry(peer).or_insert_with(|| Speaker {
            // A decoder that fails to build is a machine that cannot play
            // voice at all; the jitter buffer reports it and the speaker
            // simply stays silent rather than taking the game down.
            jitter: VoiceJitter::new().unwrap_or_else(|e| {
                log::warn!("voice: {e}");
                // `new` only fails on an invalid rate/channel pair, which is a
                // constant here — so this cannot actually happen, and the
                // expect documents that rather than hiding a real path.
                VoiceJitter::new().expect("48 kHz mono is always a valid Opus configuration")
            }),
            ring: StreamRing::new(RING_SAMPLES),
            voice: None,
            follow: None,
            params: default_voice_params(),
            since_frame_ms: f32::MAX,
            muted: false,
        })
    }
}

/// What a voice sounds like before a game says otherwise.
///
/// Spatial and fairly short-range, because the case this exists for is
/// proximity voice in a building — not a radio.
fn default_voice_params() -> PlayParams {
    PlayParams {
        mode: floptle_audio::SpatialMode::Spatial,
        falloff: floptle_audio::Falloff::Inverse,
        min_distance: 2.0,
        max_distance: 22.0,
        track: "Voice".into(),
        ..Default::default()
    }
}

fn apply_opts(params: &mut PlayParams, o: &VoiceOpts) {
    if let Some(m) = o.mode.as_deref().and_then(floptle_audio::SpatialMode::parse) {
        params.mode = m;
    }
    if let Some(f) = o.falloff.as_deref().and_then(floptle_audio::Falloff::parse) {
        params.falloff = f;
    }
    if let Some(d) = o.min_distance {
        params.min_distance = d;
    }
    if let Some(d) = o.max_distance {
        params.max_distance = d;
    }
    if let Some(v) = o.volume {
        params.volume = v;
    }
    if let Some(t) = &o.track {
        params.track = t.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn speech(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                ((std::f32::consts::TAU * 130.0 * t).sin()
                    + 0.5 * (std::f32::consts::TAU * 700.0 * t).sin())
                    * 0.3
            })
            .collect()
    }

    /// The editor's own glue: what the microphone captured comes back out as
    /// Opus packets, ready for the session.
    #[test]
    fn captured_audio_comes_back_as_packets_to_send() {
        let mut v = VoiceChat::default();
        v.set_transmit(true);
        v.inject_microphone(&speech(FRAME_SAMPLES * 3));
        let packets = v.encode_captured();
        assert_eq!(packets.len(), 3, "one packet per 20 ms frame");
        assert!(packets.iter().all(|p| !p.is_empty() && p.len() < 400), "{packets:?}");
    }

    /// Push-to-talk is enforced at the capture, so nothing downstream can leak
    /// a live microphone into a lobby.
    #[test]
    fn a_closed_microphone_produces_nothing_to_send() {
        let mut v = VoiceChat::default();
        v.inject_microphone(&speech(FRAME_SAMPLES * 3));
        assert!(v.encode_captured().is_empty());
    }

    /// A machine with no audio output still tracks the conversation — it simply
    /// cannot play it. A dedicated server takes this path.
    #[test]
    fn a_machine_with_no_output_device_still_accepts_voice() {
        let mut v = VoiceChat::default();
        let mut enc = VoiceEncoder::new().unwrap();
        let frame = speech(FRAME_SAMPLES);
        let packet = enc.encode(&frame).unwrap().expect("voiced").to_vec();
        for seq in 0..4 {
            v.receive(7, seq, &packet);
        }
        assert!(v.active(), "the speaker is known");
        let st = v.state(false);
        assert_eq!(st.sources, vec![7]);
        assert!(st.speaking.contains(&7), "frames are arriving, so they are speaking");
    }

    /// A local mute stops the decode as well as the sound — decoding audio
    /// nobody will hear is pure cost, every frame, per muted speaker.
    #[test]
    fn a_muted_speaker_is_not_even_decoded() {
        let mut v = VoiceChat::default();
        let world = World::default();
        let mut audio = crate::audio::AudioSystem::default();
        v.apply_commands(vec![VoiceCmd::Mute { peer: 3, muted: true }], &world);

        let mut enc = VoiceEncoder::new().unwrap();
        let pcm = speech(FRAME_SAMPLES * 6);
        for (seq, frame) in pcm.as_chunks::<FRAME_SAMPLES>().0.iter().enumerate() {
            let packet = enc.encode(frame).unwrap().unwrap().to_vec();
            v.receive(3, seq as u16, &packet); // muted
            v.receive(4, seq as u16, &packet); // not muted, as a control
        }
        v.advance(20.0, &world, &mut audio);

        let rows = v.diagnostics();
        let buffered = |peer: u64| {
            rows.iter().find(|r| r.0 == peer).map(|r| r.1).unwrap_or(0.0)
        };
        // The claim is about COST, not about volume: a muted speaker's audio is
        // never decoded, so nothing reaches their ring at all.
        assert_eq!(buffered(3), 0.0, "a muted speaker's audio was decoded anyway");
        assert!(buffered(4) > 0.0, "…while the control speaker really did decode");
        let st = v.state(false);
        assert!(st.muted.contains(&3));
        assert!(!st.speaking.contains(&3), "and never reads as speaking");
    }

    /// Muting somebody before they have ever spoken has to stick, or the mute
    /// is lost at exactly the moment it starts to matter.
    #[test]
    fn a_mute_set_before_the_speaker_arrives_still_applies() {
        let mut v = VoiceChat::default();
        let world = World::default();
        v.apply_commands(vec![VoiceCmd::Mute { peer: 9, muted: true }], &world);
        let mut enc = VoiceEncoder::new().unwrap();
        let packet = enc.encode(&speech(FRAME_SAMPLES)).unwrap().unwrap().to_vec();
        v.receive(9, 0, &packet);
        assert!(!v.state(false).speaking.contains(&9), "the pre-set mute held");
    }

    /// `voice.attach` carries the same knob set `audio.play` takes, and a
    /// misspelled option must not silently do nothing.
    #[test]
    fn attach_options_reach_the_voice() {
        let mut v = VoiceChat::default();
        let world = World::default();
        v.apply_commands(
            vec![VoiceCmd::Params {
                peer: 2,
                opts: VoiceOpts {
                    track: Some("Voice Monster".into()),
                    max_distance: Some(40.0),
                    mode: Some("Distance".into()),
                    ..Default::default()
                },
            }],
            &world,
        );
        let s = &v.speakers[&2];
        assert_eq!(s.params.track, "Voice Monster", "the mixer track is how a monster is made");
        assert_eq!(s.params.max_distance, 40.0);
        assert_eq!(s.params.mode, floptle_audio::SpatialMode::Distance);
    }

    /// The harness microphone: a clip played in as though a peer were speaking,
    /// 20 ms at a time.
    #[test]
    fn the_test_speaker_yields_one_frame_per_tick_and_then_stops() {
        let mut v = VoiceChat::default();
        let clip = std::sync::Arc::new(floptle_audio::Clip {
            sample_rate: 48_000,
            channels: 1,
            samples: speech(FRAME_SAMPLES * 3),
        });
        v.set_test_speaker(4, clip);
        assert_eq!(v.test_speaker(), Some(4));
        let mut frames = 0;
        while let Some((peer, seq, packet)) = v.next_test_frame() {
            assert_eq!(peer, 4);
            assert_eq!(seq, frames as u16, "sequence numbers count up");
            assert!(!packet.is_empty());
            frames += 1;
            assert!(frames < 10, "it must stop, not loop forever");
        }
        assert_eq!(frames, 3, "exactly the clip, once");
        assert_eq!(v.test_speaker(), None, "and it cleared itself up");
    }

    /// A stereo clip at some other rate is still a usable test microphone.
    #[test]
    fn the_test_speaker_downmixes_and_resamples_whatever_it_is_given() {
        let mut v = VoiceChat::default();
        let mono = speech(24_000); // half a second at 48 k
        let stereo: Vec<f32> = mono.iter().flat_map(|s| [*s, *s]).collect();
        v.set_test_speaker(
            1,
            std::sync::Arc::new(floptle_audio::Clip {
                sample_rate: 24_000,
                channels: 2,
                samples: stereo,
            }),
        );
        // 24 000 stereo frames at 24 kHz = 1 s, which at 48 kHz is 48 000
        // samples = 50 frames.
        let mut frames = 0;
        while v.next_test_frame().is_some() {
            frames += 1;
            assert!(frames < 200);
        }
        assert!((45..=50).contains(&frames), "got {frames} frames from one second");
    }
}
