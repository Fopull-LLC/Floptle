//! **Development only.** A [`Transport`] wrapper that adds latency and packet
//! loss to a real link, so a rollback match can be rehearsed at 60–120 ms RTT
//! between two editor instances on one desk.
//!
//! [`MemoryHub`](crate::MemoryHub) already simulates a bad link, but it is a
//! loopback for tests: it never touches QUIC or the relay, and its clock is the
//! gameplay tick. This is the same idea applied to the transports a real match
//! actually runs over, driven by the wall clock, and it wraps the trait rather
//! than any one implementation — so QUIC and the relay both get it for free.
//!
//! It is **off unless `FLOPTLE_NET_IMPAIR` is set** in the environment. Nothing
//! in the editor's UI can turn it on by itself; the variable makes the panel
//! section appear, and only then can it be dialled up. That asymmetry is
//! deliberate: a knob that can silently degrade a real session is worse than no
//! knob, because the resulting bug report blames the netcode.
//!
//! ## What it models, and what it does not
//!
//! Delay is applied on **send**, one way. A round trip therefore costs twice
//! the configured latency, which is why the knob is labelled one-way and the
//! panel prints the implied RTT beside it. Loss never touches
//! [`Channel::Reliable`] — a real reliable channel retransmits, and dropping
//! handshakes would only manufacture failures that cannot happen in the field.
//!
//! It does not model reordering, duplication, or bandwidth limits, and the only
//! jitter it has is an artefact: held packets are released from `poll`, so
//! delivery is quantised to the session's tick (≈16 ms at 60 Hz) and the
//! effective delay is the configured one plus up to a tick. It is a rehearsal
//! aid for "does this hold up at 100 ms", not a network emulator, and it is not
//! a substitute for the two-machine acceptance run.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::transport::{Channel, Incoming, LinkStats, PeerId, Transport};

/// The environment variable that unlocks impairment. Presence is what matters;
/// the value may optionally carry a starting setting, e.g. `50ms,2%`.
pub const IMPAIR_ENV: &str = "FLOPTLE_NET_IMPAIR";

/// Live impairment settings, shared between the wrapper and whatever is
/// driving the sliders.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Impairment {
    /// One-way delay in milliseconds. Round trip is twice this.
    pub latency_ms: u32,
    /// Fraction of unreliable packets to drop, `[0, 1]`.
    pub loss: f32,
}

impl Impairment {
    /// Is anything actually being degraded?
    pub fn is_active(self) -> bool {
        self.latency_ms > 0 || self.loss > 0.0
    }

    /// What a player would measure: twice the one-way delay.
    pub fn rtt_ms(self) -> u32 {
        self.latency_ms.saturating_mul(2)
    }

    /// Parse the environment variable's value: `"50ms,2%"`, `"50"`, `"50ms"`,
    /// or empty for "on, but not yet dialled up". Unparseable parts are
    /// ignored rather than refused — this is a dev knob, and failing to start
    /// the editor over a typo in it would be a poor trade.
    pub fn from_env_value(v: &str) -> Self {
        let mut out = Self::default();
        for part in v.split(',') {
            let p = part.trim();
            if let Some(pct) = p.strip_suffix('%') {
                if let Ok(n) = pct.trim().parse::<f32>() {
                    out.loss = (n / 100.0).clamp(0.0, 1.0);
                }
            } else {
                let n = p.strip_suffix("ms").unwrap_or(p).trim();
                if let Ok(n) = n.parse::<u32>() {
                    out.latency_ms = n.min(1000);
                }
            }
        }
        out
    }

    /// The impairment this process starts with, and whether the knob exists at
    /// all. `None` means `FLOPTLE_NET_IMPAIR` is unset: no wrapper, no panel
    /// section, nothing to go wrong.
    pub fn from_env() -> Option<Self> {
        std::env::var(IMPAIR_ENV).ok().map(|v| Self::from_env_value(&v))
    }
}

/// A handle the UI holds to retune a live link.
#[derive(Clone, Debug)]
pub struct ImpairHandle(Arc<Mutex<Impairment>>);

impl ImpairHandle {
    pub fn new(initial: Impairment) -> Self {
        Self(Arc::new(Mutex::new(initial)))
    }

    pub fn get(&self) -> Impairment {
        *self.0.lock().unwrap()
    }

    pub fn set(&self, v: Impairment) {
        *self.0.lock().unwrap() = v;
    }
}

struct Held {
    due: Instant,
    peer: PeerId,
    channel: Channel,
    bytes: Vec<u8>,
}

/// A real transport with a bad link in front of it. See the module docs.
pub struct Impaired<T: Transport> {
    inner: T,
    knob: ImpairHandle,
    held: Vec<Held>,
    rng: u64,
}

impl<T: Transport> Impaired<T> {
    pub fn new(inner: T, knob: ImpairHandle) -> Self {
        Self { inner, knob, held: Vec::new(), rng: 0x2545_F491_4F6C_DD1D }
    }

    /// xorshift64* — the drop decision has to come from somewhere, and pulling
    /// it from the clock would make two runs of the same rehearsal impossible
    /// to compare.
    fn roll(&mut self) -> f32 {
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        ((self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32) / (1 << 24) as f32
    }

    /// Hand the inner transport everything whose delay has elapsed, oldest
    /// first — the wrapper delays packets, it does not reorder them.
    fn flush(&mut self, now: Instant) {
        if self.held.is_empty() {
            return;
        }
        let (mut ready, keep): (Vec<Held>, Vec<Held>) =
            std::mem::take(&mut self.held).into_iter().partition(|h| h.due <= now);
        self.held = keep;
        ready.sort_by_key(|h| h.due);
        for h in ready {
            self.inner.send(h.peer, h.channel, &h.bytes);
        }
    }
}

impl<T: Transport> Transport for Impaired<T> {
    fn send(&mut self, peer: PeerId, channel: Channel, bytes: &[u8]) {
        let imp = self.knob.get();
        if !imp.is_active() {
            self.inner.send(peer, channel, bytes);
            return;
        }
        // A reliable channel retransmits in real life; dropping one here would
        // only invent failures the field cannot produce.
        if channel != Channel::Reliable && imp.loss > 0.0 && self.roll() < imp.loss {
            return;
        }
        if imp.latency_ms == 0 {
            self.inner.send(peer, channel, bytes);
            return;
        }
        self.held.push(Held {
            due: Instant::now() + Duration::from_millis(imp.latency_ms as u64),
            peer,
            channel,
            bytes: bytes.to_vec(),
        });
    }

    fn poll(&mut self) -> Vec<Incoming> {
        self.flush(Instant::now());
        self.inner.poll()
    }

    fn stats(&self, peer: PeerId) -> LinkStats {
        let mut s = self.inner.stats(peer);
        let imp = self.knob.get();
        // Report what the session is ACTUALLY experiencing. Lag compensation
        // and the auto input lead both read this, and telling them the
        // unimpaired truth would have them tuning for a link that isn't there.
        s.rtt_ms += imp.rtt_ms() as f32;
        s.loss = (s.loss + imp.loss).min(1.0);
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything the inner transport was actually handed.
    type Log = Arc<Mutex<Vec<(Channel, Vec<u8>)>>>;

    #[derive(Default)]
    struct Spy {
        sent: Log,
    }

    impl Transport for Spy {
        fn send(&mut self, _peer: PeerId, channel: Channel, bytes: &[u8]) {
            self.sent.lock().unwrap().push((channel, bytes.to_vec()));
        }
        fn poll(&mut self) -> Vec<Incoming> {
            Vec::new()
        }
        fn stats(&self, _peer: PeerId) -> LinkStats {
            LinkStats { rtt_ms: 10.0, loss: 0.0 }
        }
    }

    fn spy() -> (Spy, Log) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        (Spy { sent: sent.clone() }, sent)
    }

    #[test]
    fn an_unset_env_var_means_the_knob_does_not_exist() {
        // The safety property the whole design rests on: you cannot reach this
        // from the UI without having asked for it on the command line.
        assert_eq!(Impairment::from_env_value(""), Impairment::default());
        assert!(!Impairment::default().is_active());
    }

    #[test]
    fn the_env_value_parses_the_shapes_people_actually_type() {
        assert_eq!(Impairment::from_env_value("50ms,2%").latency_ms, 50);
        assert!((Impairment::from_env_value("50ms,2%").loss - 0.02).abs() < 1e-6);
        assert_eq!(Impairment::from_env_value("50").latency_ms, 50);
        assert_eq!(Impairment::from_env_value("  50 ms ").latency_ms, 50);
        // A typo must not stop the editor starting.
        assert_eq!(Impairment::from_env_value("banana").latency_ms, 0);
        assert_eq!(Impairment::from_env_value("50ms").rtt_ms(), 100, "one way, so RTT is double");
    }

    #[test]
    fn with_the_knob_at_zero_nothing_is_touched() {
        let (inner, sent) = spy();
        let mut t = Impaired::new(inner, ImpairHandle::new(Impairment::default()));
        t.send(1, Channel::Unreliable, b"hi");
        assert_eq!(sent.lock().unwrap().len(), 1, "an inactive wrapper is a pass-through");
    }

    #[test]
    fn latency_holds_a_packet_until_its_time_and_then_releases_it() {
        let (inner, sent) = spy();
        let knob = ImpairHandle::new(Impairment { latency_ms: 30, loss: 0.0 });
        let mut t = Impaired::new(inner, knob);
        t.send(1, Channel::Unreliable, b"hi");
        assert!(sent.lock().unwrap().is_empty(), "must not arrive before its delay elapses");
        t.flush(Instant::now() + Duration::from_millis(31));
        assert_eq!(sent.lock().unwrap().len(), 1, "and must arrive once it has");
    }

    #[test]
    fn loss_drops_unreliable_but_never_reliable() {
        let (inner, sent) = spy();
        let knob = ImpairHandle::new(Impairment { latency_ms: 0, loss: 1.0 });
        let mut t = Impaired::new(inner, knob);
        for _ in 0..20 {
            t.send(1, Channel::Unreliable, b"gone");
            t.send(1, Channel::Reliable, b"kept");
        }
        let got = sent.lock().unwrap();
        assert_eq!(got.len(), 20, "total loss must still deliver every reliable packet");
        assert!(got.iter().all(|(c, _)| *c == Channel::Reliable));
    }

    #[test]
    fn the_reported_link_stats_include_the_impairment() {
        // The auto input lead and lag compensation both read `stats`. Handing
        // them the unimpaired truth would have them tuning for a link that
        // isn't the one the packets are travelling over.
        let (inner, _) = spy();
        let knob = ImpairHandle::new(Impairment { latency_ms: 45, loss: 0.05 });
        let t = Impaired::new(inner, knob);
        let s = t.stats(1);
        assert!((s.rtt_ms - 100.0).abs() < 1e-3, "10 ms real + 90 ms injected, got {}", s.rtt_ms);
        assert!((s.loss - 0.05).abs() < 1e-6);
    }
}
