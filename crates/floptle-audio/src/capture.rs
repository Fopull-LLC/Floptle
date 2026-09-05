//! The microphone (`floptle/0180`).
//!
//! A cpal INPUT stream, resampled and downmixed to the one shape the rest of
//! the voice path speaks: 48 kHz mono, handed out in 20 ms frames.
//!
//! Everything here is designed around a machine that has no microphone, which
//! is most machines a game runs on. A missing input device is not an error
//! condition to be handled once at startup — it is a permanent, ordinary state
//! in which every call still has to do something sensible. So
//! [`Capture::devices`] returns an empty list, [`Capture::open`] says why in
//! one line, and `set_transmit` / `level` / `take_frame` go on working as
//! no-ops. A dedicated server is the same case: it captures nothing and plays
//! nothing, it only forwards.
//!
//! ## Transmit is a gate on the capture side, not on the sending side
//!
//! Push-to-talk is the game's decision, but where it is *enforced* is not: the
//! gate closes the moment the key comes up, before a frame is queued rather
//! than before it is sent. A gate further down the path is one that a bug
//! elsewhere can leak past, and the thing it would leak is a live microphone
//! in someone's room.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::chat::FRAME_SAMPLES;
use crate::stream::STREAM_RATE;

/// Shared state the cpal input callback writes and the game thread reads.
struct Shared {
    /// Whole 20 ms frames ready to encode.
    ///
    /// A `Mutex` rather than the lock-free ring the playback side uses: this is
    /// the *input* callback, and the only other party is the game thread once a
    /// tick. Contention is a couple of microseconds on a queue that is almost
    /// always empty, and it buys a plain `Vec` of frames rather than a second
    /// ring with a different shape.
    frames: Mutex<Vec<Vec<f32>>>,
    /// Partial frame being filled, plus the resampler's fractional position.
    partial: Mutex<(Vec<f32>, f64)>,
    /// Is the microphone open? (Push-to-talk, or a settings-screen toggle.)
    transmit: AtomicBool,
    /// Smoothed RMS of the last block, as bits of an f32 — for a level meter.
    level: AtomicU32,
}

impl Shared {
    fn new() -> Self {
        Self {
            frames: Mutex::new(Vec::new()),
            partial: Mutex::new((Vec::with_capacity(FRAME_SAMPLES), 0.0)),
            transmit: AtomicBool::new(false),
            level: AtomicU32::new(0),
        }
    }
}

/// The most frames worth queueing before dropping the oldest.
///
/// Old microphone audio is worthless: if the game has not collected for a
/// quarter of a second, sending the backlog would put every listener that far
/// behind for the rest of the conversation. Here the OLDEST is dropped, which
/// is the opposite of the playback ring's rule and right for the same reason —
/// on the way in, the freshest audio is the useful audio.
const MAX_QUEUED_FRAMES: usize = 12;

/// An open (or absent) microphone.
pub struct Capture {
    shared: Arc<Shared>,
    /// Dropping this closes the device.
    stream: Option<cpal::Stream>,
    /// The device currently open, for the settings screen.
    device_name: Option<String>,
}

impl Default for Capture {
    fn default() -> Self {
        Self::new()
    }
}

impl Capture {
    /// A capture with no device open. Never fails: a machine with no microphone
    /// is a normal machine.
    pub fn new() -> Self {
        Self { shared: Arc::new(Shared::new()), stream: None, device_name: None }
    }

    /// A device's human-readable name. cpal 0.18 reaches it through
    /// `description()` rather than the `name()` older versions had.
    fn name_of(d: &cpal::Device) -> Option<String> {
        d.description().ok().map(|d| d.name().to_string())
    }

    /// Input devices, by name. Empty on a machine with no microphone, and on a
    /// dedicated server.
    pub fn devices() -> Vec<String> {
        let host = cpal::default_host();
        let Ok(devices) = host.input_devices() else { return Vec::new() };
        devices.filter_map(|d| Self::name_of(&d)).collect()
    }

    /// The default input device's name, if there is one.
    pub fn default_device() -> Option<String> {
        cpal::default_host().default_input_device().and_then(|d| Self::name_of(&d))
    }

    /// The device currently open.
    pub fn device(&self) -> Option<&str> {
        self.device_name.as_deref()
    }

    pub fn is_open(&self) -> bool {
        self.stream.is_some()
    }

    /// Open a device by name, or the default when `name` is `None`.
    ///
    /// Replaces whatever was open. `Err` carries a line fit for a Console —
    /// callers report it and carry on, because voice is never the reason a game
    /// should fail to run.
    pub fn open(&mut self, name: Option<&str>) -> Result<(), String> {
        self.stream = None; // close the old one first: some backends are exclusive
        self.device_name = None;

        let host = cpal::default_host();
        let device = match name {
            Some(want) => host
                .input_devices()
                .map_err(|e| format!("no input devices: {e}"))?
                .find(|d| Self::name_of(d).is_some_and(|n| n == want))
                .ok_or_else(|| format!("no input device called \"{want}\""))?,
            None => host
                .default_input_device()
                .ok_or_else(|| "no microphone on this machine".to_string())?,
        };
        let device_name = Self::name_of(&device).unwrap_or_else(|| "?".into());
        let config = device
            .default_input_config()
            .map_err(|e| format!("{device_name}: no default input config ({e})"))?;
        if config.sample_format() != cpal::SampleFormat::F32 {
            return Err(format!(
                "{device_name}: input is {}, not f32",
                config.sample_format()
            ));
        }
        let channels = config.channels() as usize;
        let in_rate = config.sample_rate() as f64;
        let config: cpal::StreamConfig = config.into();

        let shared = Arc::clone(&self.shared);
        let step = in_rate / STREAM_RATE as f64;
        let stream = device
            .build_input_stream(
                config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    Self::on_input(&shared, data, channels, step);
                },
                move |e| log::warn!("microphone: {e}"),
                None,
            )
            .map_err(|e| format!("{device_name}: could not open ({e})"))?;
        stream.play().map_err(|e| format!("{device_name}: could not start ({e})"))?;
        self.stream = Some(stream);
        self.device_name = Some(device_name);
        Ok(())
    }

    /// Close the device. The capture stays usable — reopening is `open` again.
    pub fn close(&mut self) {
        self.stream = None;
        self.device_name = None;
        self.shared.level.store(0, Ordering::Relaxed);
        self.shared.frames.lock().map(|mut f| f.clear()).ok();
    }

    /// The input callback: downmix to mono, resample to 48 kHz, cut into frames.
    fn on_input(shared: &Arc<Shared>, data: &[f32], channels: usize, step: f64) {
        // Level is measured BEFORE the transmit gate, so a settings screen can
        // show the meter moving while push-to-talk is up. That is the whole
        // point of a level meter: proving the microphone works without
        // broadcasting to a lobby to find out.
        let frames_in = data.len().checked_div(channels).unwrap_or(0);
        if frames_in > 0 {
            let sum: f32 = data.iter().map(|s| s * s).sum();
            let rms = (sum / data.len() as f32).sqrt();
            let prev = f32::from_bits(shared.level.load(Ordering::Relaxed));
            // Fast attack, slow release — a meter that tracks a syllable but
            // doesn't flicker between them.
            let smoothed = if rms > prev { rms } else { prev * 0.85 + rms * 0.15 };
            shared.level.store(smoothed.to_bits(), Ordering::Relaxed);
        }
        if !shared.transmit.load(Ordering::Relaxed) {
            return;
        }
        let Ok(mut partial) = shared.partial.lock() else { return };
        let (buf, pos) = &mut *partial;
        let mut p = *pos;
        while (p as usize) < frames_in {
            let i = p as usize;
            // Downmix: the sum of the channels, not the first one — plugging
            // into the right-hand channel of a stereo interface is otherwise
            // total silence with nothing anywhere saying why.
            let mut s = 0.0;
            for c in 0..channels {
                s += data[i * channels + c];
            }
            buf.push((s / channels as f32).clamp(-1.0, 1.0));
            if buf.len() == FRAME_SAMPLES
                && let Ok(mut frames) = shared.frames.lock()
            {
                if frames.len() >= MAX_QUEUED_FRAMES {
                    // Stale microphone audio is worthless — see
                    // MAX_QUEUED_FRAMES.
                    frames.remove(0);
                }
                frames.push(std::mem::replace(buf, Vec::with_capacity(FRAME_SAMPLES)));
            }
            p += step;
        }
        *pos = p - frames_in as f64;
    }

    /// Open or close the microphone. Push-to-talk is the game's decision; this
    /// is where it takes effect.
    pub fn set_transmit(&self, on: bool) {
        self.shared.transmit.store(on, Ordering::Relaxed);
        if !on {
            // Drop the half-built frame: resuming mid-syllable would splice two
            // unrelated moments together.
            if let Ok(mut p) = self.shared.partial.lock() {
                p.0.clear();
            }
        }
    }

    pub fn transmitting(&self) -> bool {
        self.shared.transmit.load(Ordering::Relaxed)
    }

    /// The microphone's current level, 0..1 — for a settings-screen meter.
    /// Live whether or not transmit is on, and 0 with no device.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.shared.level.load(Ordering::Relaxed)).clamp(0.0, 1.0)
    }

    /// Take the captured frames waiting to be encoded. Empty is the common case.
    pub fn take_frames(&self) -> Vec<Vec<f32>> {
        self.shared.frames.lock().map(|mut f| std::mem::take(&mut *f)).unwrap_or_default()
    }

    /// TESTING / the in-editor harness: push audio in as though it had come
    /// from a microphone.
    ///
    /// This is what makes voice routing testable on one desk with no hardware
    /// — the ghost client's "microphone" is a WAV file. It respects the
    /// transmit gate exactly as a real device does, so a test that forgets to
    /// key the mic fails the same way the game would.
    pub fn inject(&self, pcm: &[f32]) {
        if !self.transmitting() {
            return;
        }
        let Ok(mut partial) = self.shared.partial.lock() else { return };
        let (buf, _) = &mut *partial;
        for s in pcm {
            buf.push(s.clamp(-1.0, 1.0));
            if buf.len() == FRAME_SAMPLES
                && let Ok(mut frames) = self.shared.frames.lock()
            {
                if frames.len() >= MAX_QUEUED_FRAMES {
                    frames.remove(0);
                }
                frames.push(std::mem::replace(buf, Vec::with_capacity(FRAME_SAMPLES)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The path every machine without a microphone takes — and a dedicated
    /// server, which has no audio device at all and must never panic on one.
    #[test]
    fn a_machine_with_no_microphone_is_an_ordinary_machine() {
        let mut cap = Capture::new();
        assert!(!cap.is_open());
        assert_eq!(cap.level(), 0.0);
        assert!(cap.take_frames().is_empty());
        cap.set_transmit(true);
        assert!(cap.take_frames().is_empty(), "nothing to capture, and no panic");
        cap.close();
        // Opening a device that isn't there is an error message, not a crash.
        assert!(cap.open(Some("definitely not a real microphone")).is_err());
        assert!(!cap.is_open());
    }

    #[test]
    fn injected_audio_becomes_whole_frames() {
        let cap = Capture::new();
        cap.set_transmit(true);
        cap.inject(&vec![0.5f32; FRAME_SAMPLES * 3]);
        let frames = cap.take_frames();
        assert_eq!(frames.len(), 3);
        assert!(frames.iter().all(|f| f.len() == FRAME_SAMPLES), "only whole frames come out");
        assert!(cap.take_frames().is_empty(), "taking them is taking them");
    }

    /// A partial frame waits for the rest rather than being padded out — a
    /// short frame would be a click, and the encoder refuses one anyway.
    #[test]
    fn a_partial_frame_waits_for_the_rest_of_itself() {
        let cap = Capture::new();
        cap.set_transmit(true);
        cap.inject(&vec![0.5f32; FRAME_SAMPLES / 2]);
        assert!(cap.take_frames().is_empty());
        cap.inject(&vec![0.5f32; FRAME_SAMPLES / 2]);
        assert_eq!(cap.take_frames().len(), 1, "the two halves made one frame");
    }

    /// The gate is on the capture side, so a bug further down the path cannot
    /// leak a live microphone into a lobby.
    #[test]
    fn nothing_is_captured_while_the_mic_is_closed() {
        let cap = Capture::new();
        cap.inject(&vec![0.9f32; FRAME_SAMPLES * 2]);
        assert!(cap.take_frames().is_empty(), "push-to-talk was up");
        cap.set_transmit(true);
        cap.inject(&vec![0.9f32; FRAME_SAMPLES]);
        assert_eq!(cap.take_frames().len(), 1);
    }

    /// Releasing the key drops the half-built frame: resuming later must not
    /// splice two unrelated moments together.
    #[test]
    fn releasing_the_key_discards_the_half_built_frame() {
        let cap = Capture::new();
        cap.set_transmit(true);
        cap.inject(&vec![0.5f32; FRAME_SAMPLES / 2]);
        cap.set_transmit(false);
        cap.set_transmit(true);
        cap.inject(&vec![0.5f32; FRAME_SAMPLES / 2]);
        assert!(cap.take_frames().is_empty(), "the two halves were not spliced together");
    }

    /// A game that stops collecting must not build a backlog: old microphone
    /// audio is worse than no microphone audio, because sending it puts every
    /// listener permanently behind.
    #[test]
    fn a_backlog_drops_the_oldest_audio_not_the_newest() {
        let cap = Capture::new();
        cap.set_transmit(true);
        // Real sample values: capture clamps to -1..1, so a counter would come
        // back as a column of 1.0 and prove nothing about ordering.
        let tag = |i: usize| i as f32 / 100.0;
        for i in 0..(MAX_QUEUED_FRAMES + 5) {
            cap.inject(&vec![tag(i); FRAME_SAMPLES]);
        }
        let frames = cap.take_frames();
        assert_eq!(frames.len(), MAX_QUEUED_FRAMES, "the queue is bounded");
        assert_eq!(
            frames.last().unwrap()[0],
            tag(MAX_QUEUED_FRAMES + 4),
            "the freshest audio survived — it is the only kind worth sending"
        );
        assert_eq!(
            frames.first().unwrap()[0],
            tag(5),
            "…and it was the OLDEST that went, not the newest that was refused"
        );
    }
}
