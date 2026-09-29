//! Bringing a microphone to a steady speaking level before it is sent.
//!
//! A microphone arrives wherever its owner's gain knob left it, typically
//! -30 to -20 dBFS of speech against game sound mastered near 0 dBFS, so a
//! remote player is too quiet on one machine and clipping on the next. The
//! leveller measures each 20 ms frame and moves a gain toward the one that
//! puts speech at [`TARGET_DBFS`]: down quickly, up slowly, and not at all
//! while nobody is speaking, so the room's hiss between sentences is never
//! pumped up to speech level. A soft limiter catches what the gain overshoots.

use crate::effects::db_to_lin;

/// Where speech lands, as RMS.
pub const TARGET_DBFS: f32 = -18.0;
/// The most the leveller will raise a quiet microphone.
pub const MAX_GAIN_DB: f32 = 24.0;
/// The most it will lower a loud one.
pub const MIN_GAIN_DB: f32 = -12.0;
/// Below this a frame is not speech, and the gain holds.
pub const GATE_DBFS: f32 = -50.0;

/// The leveller's state, one per microphone.
#[derive(Debug, Clone)]
pub struct VoiceLeveler {
    /// Level toward speech at [`TARGET_DBFS`] (on by default).
    pub auto: bool,
    /// A fixed gain in dB, applied before the leveller (or alone, when it is off).
    pub input_db: f32,
    gain: f32,
}

impl Default for VoiceLeveler {
    fn default() -> Self {
        Self { auto: true, input_db: 0.0, gain: 1.0 }
    }
}

impl VoiceLeveler {
    /// The gain the leveller is at now, in dB.
    pub fn gain_db(&self) -> f32 {
        20.0 * self.gain.max(1e-6).log10()
    }

    /// Level one frame in place.
    pub fn process(&mut self, frame: &mut [f32]) {
        if frame.is_empty() {
            return;
        }
        let input = db_to_lin(self.input_db);
        let rms = (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt() * input;
        let from = self.gain;
        if self.auto && rms > db_to_lin(GATE_DBFS) {
            let want = (db_to_lin(TARGET_DBFS) / rms).clamp(db_to_lin(MIN_GAIN_DB), db_to_lin(MAX_GAIN_DB));
            // Down within a frame or two, up over about a second: a shout is
            // caught at once, and a quiet sentence is not lifted in the gap
            // before it.
            let rate = if want < self.gain { 0.5 } else { 0.05 };
            self.gain += (want - self.gain) * rate;
        } else if !self.auto {
            self.gain = 1.0;
        }
        let step = (self.gain - from) / frame.len() as f32;
        for (i, s) in frame.iter_mut().enumerate() {
            let x = *s * input * (from + step * i as f32);
            *s = soft_limit(x);
        }
    }
}

/// Unity up to 0.8, then bending smoothly toward 1 so nothing clips.
fn soft_limit(x: f32) -> f32 {
    const KNEE: f32 = 0.8;
    let a = x.abs();
    if a <= KNEE {
        x
    } else {
        let over = (a - KNEE) / (1.0 - KNEE);
        (KNEE + (1.0 - KNEE) * over.tanh()).copysign(x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn speech(amp: f32) -> Vec<f32> {
        (0..960)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                ((std::f32::consts::TAU * 140.0 * t).sin() + 0.4 * (std::f32::consts::TAU * 900.0 * t).sin()) * amp
            })
            .collect()
    }

    fn rms_db(f: &[f32]) -> f32 {
        let r = (f.iter().map(|s| s * s).sum::<f32>() / f.len() as f32).sqrt();
        20.0 * r.max(1e-9).log10()
    }

    /// **A quiet microphone and a loud one end up at the same level.** Two
    /// seconds of speech at -32 and at -8 dBFS both settle within 2 dB of the
    /// target, and nothing leaves above full scale.
    #[test]
    fn quiet_and_loud_speech_settle_at_the_target() {
        for amp in [0.025f32, 0.4] {
            let mut lv = VoiceLeveler::default();
            let mut last = Vec::new();
            for _ in 0..100 {
                let mut f = speech(amp);
                lv.process(&mut f);
                assert!(f.iter().all(|s| s.abs() <= 1.0), "a sample clipped");
                last = f;
            }
            let before = rms_db(&speech(amp));
            let after = rms_db(&last);
            assert!((after - TARGET_DBFS).abs() < 2.0, "speech at {before:.1} dBFS came out at {after:.1}");
        }
    }

    /// **Silence between sentences is not lifted.** A quiet voice has pushed
    /// the gain up; the near-silent hiss that follows leaves at the gain the
    /// voice had, not at speech level.
    #[test]
    fn the_gain_holds_through_silence() {
        let mut lv = VoiceLeveler::default();
        for _ in 0..100 {
            lv.process(&mut speech(0.025));
        }
        let g = lv.gain_db();
        let mut hiss = vec![0.0005f32; 960];
        for _ in 0..100 {
            hiss.fill(0.0005);
            lv.process(&mut hiss);
        }
        assert!((lv.gain_db() - g).abs() < 0.01, "the gain moved from {g:.1} to {:.1} dB in silence", lv.gain_db());
        assert!(rms_db(&hiss) < -30.0, "the hiss came out at {:.1} dBFS", rms_db(&hiss));
    }

    /// Off, it passes the sound through at the fixed input gain alone.
    #[test]
    fn off_it_is_the_input_gain_alone() {
        let mut lv = VoiceLeveler { auto: false, input_db: 6.0, ..Default::default() };
        let mut f = speech(0.05);
        lv.process(&mut f);
        let got = rms_db(&f) - rms_db(&speech(0.05));
        assert!((got - 6.0).abs() < 0.1, "{got}");
    }
}
