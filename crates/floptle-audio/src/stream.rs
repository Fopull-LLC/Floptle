//! A live sample stream feeding a playing voice — what makes a remote player's
//! microphone an ordinary sound in the world (`floptle/0180`).
//!
//! Every other sound in the engine is a [`Clip`](crate::clip::Clip): a fully
//! decoded buffer that exists before it plays. A voice stream is the opposite —
//! it arrives 20 ms at a time, forever, and the frame that has not arrived yet
//! is the normal case rather than an error. So it needs a source the mixer can
//! read from while something else is still writing to it.
//!
//! ## Why a ring of atomics rather than a lock or a `VecDeque`
//!
//! The producer is a control thread (the session, decoding Opus as datagrams
//! land) and the consumer is the cpal callback. That callback must never block:
//! waiting on a mutex held by a thread the scheduler has just descheduled is a
//! buffer underrun on every listener at once, heard as a click. This crate has
//! no `unsafe` and is not going to acquire any for this, so the ring is a
//! fixed `Box<[AtomicU32]>` of bit-cast samples with two cursors — lock-free,
//! wait-free, and safe. At 48 kHz mono one stream costs 48 000 relaxed atomic
//! loads a second, which is nothing next to the spatialisation it feeds.
//!
//! Single producer, single consumer. That is not a limitation to design around:
//! one remote speaker is one network stream and one voice.
//!
//! ## Running dry is not an error
//!
//! A gap in the network is a gap in the sound. [`StreamRing::pop`] returns
//! `None`, the voice emits silence, and [`StreamRing::starved`] counts it so
//! the jitter buffer can widen. The one thing that must never happen is the
//! stream ENDING because a packet was late — a voice that finished cannot be
//! resumed, and the player would go permanently silent halfway through a
//! sentence. Nothing in here can mark a voice done.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

/// A shared live stream. Cheap to clone — both ends hold the same ring.
pub type StreamRef = Arc<StreamRing>;

/// Sample rate every voice stream is carried at. Opus is a 48 kHz codec and
/// resampling twice to please a device that wants 44.1 kHz would cost quality
/// for nothing — the voice resamples once, on the way out, exactly as a clip
/// recorded at some other rate already does.
pub const STREAM_RATE: u32 = 48_000;

/// A lock-free single-producer single-consumer ring of mono samples.
pub struct StreamRing {
    buf: Box<[AtomicU32]>,
    /// Always a power of two, so the wrap is a mask rather than a modulo.
    mask: usize,
    write: AtomicUsize,
    read: AtomicUsize,
    /// Samples the consumer asked for and did not get.
    starved: AtomicU64,
    /// Samples the ring had no room for. See [`StreamRing::refused`].
    refused: AtomicU64,
}

impl StreamRing {
    /// A ring holding at least `capacity` samples (rounded up to a power of
    /// two). Sized by the caller from the worst jitter it intends to absorb.
    pub fn new(capacity: usize) -> StreamRef {
        let cap = capacity.next_power_of_two().max(2);
        Arc::new(Self {
            buf: (0..cap).map(|_| AtomicU32::new(0)).collect::<Vec<_>>().into_boxed_slice(),
            mask: cap - 1,
            write: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
            starved: AtomicU64::new(0),
            refused: AtomicU64::new(0),
        })
    }

    /// Milliseconds of audio the ring can hold at [`STREAM_RATE`].
    pub fn capacity_ms(&self) -> f32 {
        self.buf.len() as f32 * 1000.0 / STREAM_RATE as f32
    }

    /// Samples waiting to be played.
    pub fn len(&self) -> usize {
        self.write.load(Ordering::Acquire).wrapping_sub(self.read.load(Ordering::Acquire))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Milliseconds of audio buffered — the number the jitter buffer steers.
    pub fn buffered_ms(&self) -> f32 {
        self.len() as f32 * 1000.0 / STREAM_RATE as f32
    }

    /// PRODUCER: append samples. Returns how many were taken; a short return
    /// means the ring is full.
    ///
    /// A full ring means the far end is sending faster than this machine plays,
    /// which is a clock-drift problem rather than a moment's congestion. The
    /// oldest audio is NOT discarded to make room: dropping the front would
    /// skip the listener forward through the middle of a word. The newest is
    /// refused instead, the count is kept, and the jitter buffer above shrinks
    /// its target so the backlog drains.
    pub fn push(&self, samples: &[f32]) -> usize {
        let cap = self.buf.len();
        let w = self.write.load(Ordering::Relaxed);
        let r = self.read.load(Ordering::Acquire);
        let free = cap - w.wrapping_sub(r);
        let take = samples.len().min(free);
        for (i, s) in samples[..take].iter().enumerate() {
            self.buf[w.wrapping_add(i) & self.mask].store(s.to_bits(), Ordering::Relaxed);
        }
        self.write.store(w.wrapping_add(take), Ordering::Release);
        let missed = samples.len() - take;
        if missed > 0 {
            self.refused.fetch_add(missed as u64, Ordering::Relaxed);
        }
        take
    }

    /// CONSUMER: take the next sample, or `None` if none has arrived.
    ///
    /// Counted when it comes up empty, because "the voice went quiet" and "the
    /// network went quiet" are indistinguishable to a listener and very
    /// different to whoever is debugging it.
    pub fn pop(&self) -> Option<f32> {
        let r = self.read.load(Ordering::Relaxed);
        if r == self.write.load(Ordering::Acquire) {
            self.starved.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let s = f32::from_bits(self.buf[r & self.mask].load(Ordering::Relaxed));
        self.read.store(r.wrapping_add(1), Ordering::Release);
        Some(s)
    }

    /// CONSUMER: throw away everything buffered — a hard resync after a long
    /// stall, where playing the backlog would only add the stall to the delay.
    pub fn clear(&self) {
        self.read.store(self.write.load(Ordering::Acquire), Ordering::Release);
    }

    /// Samples the consumer wanted and did not get, for the whole life of the
    /// stream.
    pub fn starved(&self) -> u64 {
        self.starved.load(Ordering::Relaxed)
    }

    /// Samples the ring had no room for.
    ///
    /// **Refused is not the same as lost.** `push` reports how many it took, so
    /// a producer that retries the remainder loses nothing and still counts
    /// here — this is a measure of how often the ring ran full, which is a
    /// clock-drift or scheduling signal. Whether audio was actually lost is the
    /// caller's business, because only the caller knows if it came back.
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }
}

impl std::fmt::Debug for StreamRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamRing")
            .field("buffered_ms", &self.buffered_ms())
            .field("capacity_ms", &self.capacity_ms())
            .field("starved", &self.starved())
            .field("refused", &self.refused())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_come_out_in_the_order_they_went_in() {
        let ring = StreamRing::new(64);
        assert!(ring.is_empty());
        ring.push(&[0.25, -0.5, 0.75]);
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.pop(), Some(0.25));
        assert_eq!(ring.pop(), Some(-0.5));
        assert_eq!(ring.pop(), Some(0.75));
        assert_eq!(ring.pop(), None);
    }

    /// The gap case, which is the normal one on a real network.
    #[test]
    fn running_dry_is_reported_not_hidden() {
        let ring = StreamRing::new(16);
        assert_eq!(ring.pop(), None);
        assert_eq!(ring.pop(), None);
        assert_eq!(ring.starved(), 2, "a listener cannot tell silence from loss; this can");
    }

    /// A full ring refuses the NEWEST audio. Dropping the oldest would skip the
    /// listener forward through the middle of a word — the artefact is far
    /// worse than the delay it saves, and it compounds every time it happens.
    #[test]
    fn a_full_ring_refuses_new_audio_rather_than_discarding_old() {
        let ring = StreamRing::new(4);
        assert_eq!(ring.push(&[1.0, 2.0, 3.0, 4.0]), 4);
        assert_eq!(ring.push(&[5.0, 6.0]), 0, "no room");
        assert_eq!(ring.refused(), 2);
        assert_eq!(ring.pop(), Some(1.0), "the oldest audio survived");
    }

    #[test]
    fn the_ring_wraps_without_losing_anything() {
        let ring = StreamRing::new(8);
        // Many times round, so the cursors pass the capacity repeatedly.
        for round in 0..200u32 {
            let v = round as f32;
            assert_eq!(ring.push(&[v, v + 0.5]), 2);
            assert_eq!(ring.pop(), Some(v));
            assert_eq!(ring.pop(), Some(v + 0.5));
        }
        assert_eq!(ring.starved(), 0);
        assert_eq!(ring.refused(), 0);
    }

    #[test]
    fn clearing_drops_the_backlog_and_leaves_the_stream_usable() {
        let ring = StreamRing::new(64);
        ring.push(&[1.0; 32]);
        ring.clear();
        assert!(ring.is_empty());
        ring.push(&[9.0]);
        assert_eq!(ring.pop(), Some(9.0), "the stream keeps going — it did not end");
    }

    #[test]
    fn buffered_ms_tracks_the_stream_rate() {
        let ring = StreamRing::new(4096);
        ring.push(&vec![0.0; 960]); // one 20 ms frame
        assert!((ring.buffered_ms() - 20.0).abs() < 0.01, "{}", ring.buffered_ms());
    }

    /// Written and read from two real threads, because the whole point of the
    /// structure is that those are different threads.
    #[test]
    fn a_producer_and_a_consumer_on_separate_threads_agree() {
        let ring = StreamRing::new(1024);
        let producer = Arc::clone(&ring);
        const N: usize = 50_000;
        let t = std::thread::spawn(move || {
            let mut sent = 0usize;
            while sent < N {
                let v = [sent as f32];
                if producer.push(&v) == 1 {
                    sent += 1;
                } else {
                    std::thread::yield_now();
                }
            }
        });
        let mut got = 0usize;
        while got < N {
            match ring.pop() {
                Some(s) => {
                    assert_eq!(s, got as f32, "out of order at {got}");
                    got += 1;
                }
                None => std::thread::yield_now(),
            }
        }
        t.join().unwrap();
        // Every one of the N samples arrived, in order, which is the claim.
        // The ring running full along the way is ordinary backpressure — the
        // producer retried and nothing was lost, which is exactly why `refused`
        // is not called "dropped".
        assert_eq!(got, N);
    }
}
