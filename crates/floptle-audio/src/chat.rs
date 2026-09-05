//! Voice chat: the codec and the jitter buffer (`floptle/0180`).
//!
//! The path a spoken word takes is: microphone → [`VoiceEncoder`] → one 20 ms
//! Opus packet on an unreliable datagram → the server forwards it → the
//! listener's [`VoiceJitter`] puts it back in order → [`VoiceDecoder`] →
//! a [`StreamRing`](crate::stream::StreamRing) → an ordinary spatial voice in
//! the mixer.
//!
//! Capture lives in [`capture`](crate::capture); everything here is pure and
//! testable with no device, which is what lets the whole routing story be
//! proven on one desk.
//!
//! ## Why the jitter buffer is the interesting part
//!
//! A network delivers 20 ms packets at 20 ms intervals only on average. Play
//! them the instant they arrive and every jitter spike is an audible gap; hold
//! a fat buffer and the conversation develops a delay people talk over. So the
//! buffer holds the smallest cushion that has been getting away with it lately,
//! and re-measures continuously.
//!
//! Three things it must get right, all learned from how voice actually fails:
//!
//! * **Late is not lost.** Datagrams reorder. A packet that arrives after its
//!   successor is still playable if the cushion has not drained past it.
//! * **A gap is silence, never a stall.** Waiting for a packet that may never
//!   come turns one lost datagram into a permanent delay. Opus conceals the
//!   gap and playback continues.
//! * **The cushion must be able to shrink.** A buffer that only ever grows
//!   ends the match at a second of latency because of one bad moment near the
//!   start.

use crate::stream::{StreamRef, STREAM_RATE};

/// Samples in one voice frame — 20 ms at 48 kHz.
///
/// 20 ms is the standard voice trade: 10 ms would double the packet rate and
/// the per-packet header overhead for a barely perceptible latency win, and
/// 40 ms makes every loss twice as audible.
pub const FRAME_SAMPLES: usize = 960;

/// Milliseconds of audio in one frame.
pub const FRAME_MS: f32 = 20.0;

/// Target bitrate. Speech at 24 kbps mono is clean; eight players talking at
/// once is under 200 kbps of forwarding, which is nothing.
pub const BITRATE: i32 = 24_000;

/// The largest Opus packet this ever produces, for a fixed scratch buffer.
pub const MAX_PACKET: usize = 400;

/// Encodes captured microphone audio into 20 ms Opus packets.
pub struct VoiceEncoder {
    inner: opus_pure::OpusEncoder,
    packet: Vec<u8>,
}

impl VoiceEncoder {
    pub fn new() -> Result<Self, String> {
        let mut inner =
            opus_pure::OpusEncoder::new(STREAM_RATE as i32, 1, opus_pure::Application::Voip)
                .map_err(|e| format!("voice encoder: {e}"))?;
        inner.bitrate_bps = BITRATE;
        // Tell the encoder to expect loss, so it spends a few bits on in-band
        // FEC rather than producing a stream that falls apart the moment a
        // datagram goes missing. 5% is a realistic figure for a home
        // connection; the decoder's concealment covers the rest.
        inner.packet_loss_perc = 5;
        // Don't transmit silence. Push-to-talk already gates most of it, but an
        // open mic in a quiet room is otherwise a steady 24 kbps of nothing —
        // per player, forwarded to every other player.
        inner.use_dtx = true;
        Ok(Self { inner, packet: vec![0u8; MAX_PACKET] })
    }

    /// Encode exactly one frame. `pcm` must be [`FRAME_SAMPLES`] long.
    ///
    /// `Ok(None)` means the encoder decided this frame is silence and produced
    /// nothing worth sending (DTX) — not an error, and not something to send an
    /// empty packet for.
    pub fn encode(&mut self, pcm: &[f32]) -> Result<Option<&[u8]>, String> {
        if pcm.len() != FRAME_SAMPLES {
            return Err(format!("voice frame must be {FRAME_SAMPLES} samples, got {}", pcm.len()));
        }
        let n = self
            .inner
            .encode(pcm, FRAME_SAMPLES, &mut self.packet)
            .map_err(|e| format!("voice encode: {e}"))?;
        // Opus signals "nothing to send" with a 1-2 byte packet under DTX.
        Ok((n > 2).then_some(&self.packet[..n]))
    }
}

/// Decodes one remote speaker's packets, concealing what never arrived.
pub struct VoiceDecoder {
    inner: opus_pure::OpusDecoder,
    pcm: Vec<f32>,
}

impl VoiceDecoder {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            inner: opus_pure::OpusDecoder::new(STREAM_RATE as i32, 1)
                .map_err(|e| format!("voice decoder: {e}"))?,
            pcm: vec![0.0; FRAME_SAMPLES],
        })
    }

    /// Decode one packet, or conceal a lost one (`None`).
    ///
    /// Concealment is not silence: Opus extrapolates the waveform it was in the
    /// middle of, so a single dropped packet is a slight roughness rather than
    /// a hole punched in a word.
    pub fn decode(&mut self, packet: Option<&[u8]>) -> Result<&[f32], String> {
        let n = match packet {
            Some(p) => self.inner.decode(p, FRAME_SAMPLES, &mut self.pcm),
            None => self.inner.decode(&[], FRAME_SAMPLES, &mut self.pcm),
        }
        .map_err(|e| format!("voice decode: {e}"))?;
        Ok(&self.pcm[..n])
    }
}

/// The smallest cushion worth holding, and the largest the card allows.
const MIN_DEPTH_MS: f32 = 20.0;
const MAX_DEPTH_MS: f32 = 60.0;

/// Reorders one speaker's packets and releases them on time.
///
/// One of these per remote speaker, on the listening machine.
pub struct VoiceJitter {
    /// Packets waiting, oldest first, as (sequence, payload).
    held: Vec<(u16, Vec<u8>)>,
    /// The next sequence number to play. `None` until the first packet lands.
    next: Option<u16>,
    /// How much cushion to hold, in milliseconds. Adaptive within
    /// [`MIN_DEPTH_MS`]..=[`MAX_DEPTH_MS`].
    depth_ms: f32,
    decoder: VoiceDecoder,
    /// Packets that arrived after their slot had already played.
    too_late: u64,
    /// Slots played as concealment because nothing arrived in time.
    concealed: u64,
    /// Frames released into the ring.
    played: u64,
}

impl VoiceJitter {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            held: Vec::new(),
            next: None,
            depth_ms: MIN_DEPTH_MS + FRAME_MS,
            decoder: VoiceDecoder::new()?,
            too_late: 0,
            concealed: 0,
            played: 0,
        })
    }

    /// A packet arrived. `seq` is the speaker's frame counter.
    pub fn accept(&mut self, seq: u16, payload: &[u8]) {
        if let Some(next) = self.next {
            // Wrapping-aware "already gone past this". A u16 counter wraps
            // every 22 minutes of continuous speech, so comparing with `<`
            // would throw away twenty minutes of audio at the wrap.
            let age = next.wrapping_sub(seq);
            if age > 0 && age < u16::MAX / 2 {
                self.too_late += 1;
                // Arriving late repeatedly means the cushion is too thin.
                self.widen();
                return;
            }
        }
        if self.held.iter().any(|(s, _)| *s == seq) {
            return; // duplicate
        }
        self.held.push((seq, payload.to_vec()));
        self.held.sort_by_key(|(s, _)| *s);
    }

    /// Release whatever is due into `ring`, decoding as it goes.
    ///
    /// Called once per game tick. Returns how many frames were released.
    pub fn drain_into(&mut self, ring: &StreamRef) -> usize {
        let mut released = 0;
        // Keep going while the ring is running thinner than the target cushion.
        while ring.buffered_ms() < self.depth_ms {
            let Some(next) = self.next else {
                // Nothing has ever played: wait for enough to have arrived that
                // the first gap doesn't happen immediately.
                if (self.held.len() as f32 * FRAME_MS) < self.depth_ms {
                    return released;
                }
                self.next = self.held.first().map(|(s, _)| *s);
                continue;
            };
            let take = self.held.iter().position(|(s, _)| *s == next);
            let pcm = match take {
                Some(i) => {
                    let (_, payload) = self.held.remove(i);
                    self.decoder.decode(Some(&payload))
                }
                // Nothing at all is waiting: the speaker stopped talking, or
                // the link went away. Neither is a lost packet, and concealing
                // them would manufacture artefacts out of an ordinary silence
                // — forever, since nothing would ever arrive to stop it.
                None if self.held.is_empty() => return released,
                None => {
                    // The slot is due, its packet is not here, and something
                    // NEWER is — so it is genuinely missing rather than merely
                    // not sent yet. Conceal and move on: waiting for it would
                    // turn one lost datagram into a permanent delay for the
                    // rest of the session.
                    self.concealed += 1;
                    self.decoder.decode(None)
                }
            };
            match pcm {
                Ok(samples) => {
                    ring.push(samples);
                    released += 1;
                    self.played += 1;
                }
                Err(e) => {
                    log::warn!("voice: {e}");
                    self.held.clear();
                    return released;
                }
            }
            self.next = Some(next.wrapping_add(1));
        }
        // A healthy stretch earns a smaller cushion back. Slowly: latency that
        // ratchets up on every hiccup and never comes down ends the match a
        // second behind, and a buffer that shrinks eagerly just widens again.
        if self.concealed == 0 && self.too_late == 0 {
            self.depth_ms = (self.depth_ms - 0.05).max(MIN_DEPTH_MS);
        }
        released
    }

    fn widen(&mut self) {
        self.depth_ms = (self.depth_ms + FRAME_MS).min(MAX_DEPTH_MS);
    }

    /// The cushion currently being held, milliseconds.
    pub fn depth_ms(&self) -> f32 {
        self.depth_ms
    }

    /// Packets that arrived after their slot had played.
    pub fn too_late(&self) -> u64 {
        self.too_late
    }

    /// Slots filled by concealment because nothing arrived in time.
    pub fn concealed(&self) -> u64 {
        self.concealed
    }

    pub fn played(&self) -> u64 {
        self.played
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::StreamRing;

    /// A short voiced buzz — enough structure that the codec has something to
    /// do, unlike silence, which DTX would (correctly) refuse to send.
    fn speech(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = i as f32 / STREAM_RATE as f32;
                ((std::f32::consts::TAU * 130.0 * t).sin()
                    + 0.5 * (std::f32::consts::TAU * 700.0 * t).sin())
                    * 0.3
            })
            .collect()
    }

    #[test]
    fn a_frame_survives_the_round_trip() {
        let mut enc = VoiceEncoder::new().unwrap();
        let mut dec = VoiceDecoder::new().unwrap();
        let pcm = speech(FRAME_SAMPLES);
        let packet = enc.encode(&pcm).unwrap().expect("speech is not silence").to_vec();
        assert!(packet.len() < MAX_PACKET, "packet {} B", packet.len());
        let out = dec.decode(Some(&packet)).unwrap();
        assert_eq!(out.len(), FRAME_SAMPLES, "a full frame comes back");
    }

    /// The codec must hit the budget the card set, not merely work.
    #[test]
    fn the_stream_costs_about_the_advertised_bitrate() {
        let mut enc = VoiceEncoder::new().unwrap();
        let pcm = speech(FRAME_SAMPLES * 100); // 2 s
        let mut bytes = 0;
        for f in pcm.as_chunks::<FRAME_SAMPLES>().0 {
            if let Some(p) = enc.encode(f).unwrap() {
                bytes += p.len();
            }
        }
        let kbps = bytes as f32 * 8.0 / 2.0 / 1000.0;
        assert!((10.0..32.0).contains(&kbps), "{kbps:.1} kbps is not ~24");
    }

    #[test]
    fn a_wrong_sized_frame_is_refused_rather_than_half_encoded() {
        let mut enc = VoiceEncoder::new().unwrap();
        assert!(enc.encode(&[0.0; 100]).is_err());
    }

    /// Run the real loop: each tick the jitter buffer tops the cushion up, then
    /// the audio thread consumes a frame's worth. Without the consumer the
    /// buffer correctly fills its cushion and stops, which is the behaviour —
    /// not something a test should paper over by draining in one go.
    /// Returns everything the "audio thread" actually consumed — which is what
    /// a listener would have heard, and the only honest thing to assert on.
    fn play(j: &mut VoiceJitter, ring: &StreamRef, ticks: usize) -> Vec<f32> {
        let mut heard = Vec::new();
        for _ in 0..ticks {
            j.drain_into(ring);
            for _ in 0..FRAME_SAMPLES {
                if let Some(s) = ring.pop() {
                    heard.push(s);
                }
            }
        }
        heard
    }

    /// Peak level of what was heard — zero means the listener got silence.
    fn peak(samples: &[f32]) -> f32 {
        samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max)
    }

    fn packets(n: usize) -> Vec<Vec<u8>> {
        let mut enc = VoiceEncoder::new().unwrap();
        let pcm = speech(FRAME_SAMPLES * n);
        pcm.as_chunks::<FRAME_SAMPLES>()
            .0
            .iter()
            .map(|f| enc.encode(f).unwrap().expect("voiced").to_vec())
            .collect()
    }

    /// Datagrams reorder. A packet that overtook its neighbour is still
    /// perfectly playable, and throwing it away would be a self-inflicted drop.
    #[test]
    fn packets_that_arrive_out_of_order_are_put_back_in_order() {
        let ring = StreamRing::new(48_000);
        let mut j = VoiceJitter::new().unwrap();
        let ps = packets(8);
        // 0, 2, 1, 3, 5, 4, 6, 7 — neighbours swapped, as a real link does.
        for i in [0usize, 2, 1, 3, 5, 4, 6, 7] {
            j.accept(i as u16, &ps[i]);
        }
        let heard = play(&mut j, &ring, 8);
        assert!(peak(&heard) > 0.05, "the listener heard the speech");
        assert_eq!(j.concealed(), 0, "nothing was concealed: everything was there");
        assert_eq!(j.too_late(), 0, "and nothing was judged late");
        assert!(j.played() >= 4, "played {}", j.played());
    }

    /// The one that must never regress into waiting: a lost packet is a gap in
    /// the sound, not a stall in the stream.
    #[test]
    fn a_lost_packet_is_concealed_and_playback_continues() {
        let ring = StreamRing::new(48_000);
        let mut j = VoiceJitter::new().unwrap();
        let ps = packets(10);
        for (i, p) in ps.iter().enumerate() {
            if i == 4 {
                continue; // this one never arrives
            }
            j.accept(i as u16, p);
        }
        let heard = play(&mut j, &ring, 10);
        assert!(j.played() >= 6, "playback continued past the hole: {}", j.played());
        assert_eq!(j.concealed(), 1, "exactly the missing slot was concealed");
        assert!(peak(&heard) > 0.05, "the listener heard real speech, not silence");
        assert!(
            heard.len() >= FRAME_SAMPLES * 6,
            "and heard most of it: {} samples",
            heard.len()
        );
    }

    /// A u16 sequence wraps every ~22 minutes of speech. Treating the wrap as
    /// "ancient" would throw away twenty minutes of conversation at a stroke.
    #[test]
    fn the_sequence_counter_wrapping_does_not_discard_the_stream() {
        let ring = StreamRing::new(48_000);
        let mut j = VoiceJitter::new().unwrap();
        let ps = packets(7);

        // Get playback genuinely under way first, and leave the read head at
        // 65535 — the ONE position where a naive `seq < next` misjudges the
        // next packet. Feeding everything up front leaves `next` unset and
        // exercises none of this.
        j.accept(u16::MAX - 2, &ps[0]);
        j.accept(u16::MAX - 1, &ps[1]);
        play(&mut j, &ring, 2);
        assert!(j.played() >= 2, "playback started before the wrap");

        // …and now it rolls over: 65535, then 0, 1, 2, 3.
        j.accept(u16::MAX, &ps[2]);
        for (k, p) in ps.iter().skip(3).enumerate() {
            j.accept(k as u16, p);
        }
        let heard = play(&mut j, &ring, 8);
        assert_eq!(
            j.too_late(),
            0,
            "sequence 0 arriving after 65535 is the NEXT packet, not one that is \
             twenty minutes old"
        );
        assert!(peak(&heard) > 0.05, "audio kept flowing across the wrap");
        assert!(j.played() >= 6, "played {} across the wrap", j.played());
    }

    #[test]
    fn a_duplicate_packet_is_ignored() {
        let ring = StreamRing::new(48_000);
        let mut j = VoiceJitter::new().unwrap();
        let ps = packets(4);
        for (i, p) in ps.iter().enumerate() {
            j.accept(i as u16, p);
            j.accept(i as u16, p); // the retransmit that isn't
        }
        play(&mut j, &ring, 6);
        assert!(j.played() <= 4, "a duplicate must not become a second frame");
    }

    /// A packet arriving after its slot has played widens the cushion — and the
    /// cushion is bounded, because latency is the thing this is trading against.
    #[test]
    fn persistent_lateness_widens_the_cushion_but_only_so_far() {
        let mut j = VoiceJitter::new().unwrap();
        let start = j.depth_ms();
        let ring = StreamRing::new(48_000);
        let ps = packets(4);
        for (i, p) in ps.iter().enumerate() {
            j.accept(i as u16, p);
        }
        play(&mut j, &ring, 4);
        for _ in 0..20 {
            j.accept(0, &ps[0]); // ancient, every time
        }
        assert!(j.depth_ms() > start, "a link that keeps arriving late earns more cushion");
        assert!(j.depth_ms() <= MAX_DEPTH_MS, "…but never past the ceiling: {}", j.depth_ms());
        assert!(j.too_late() >= 20);
    }
}
