//! Time-preserving pitch shifter: the classic dual-tap crossfaded delay line.
//!
//! Two read taps sweep through a short window at a rate proportional to the
//! pitch ratio, half a window apart, crossfaded with an equal-power triangle
//! so neither tap is audible while it jumps back to the start of the window.

use super::{EffectDesc, EffectDsp};

const MAX_WINDOW_MS: f32 = 250.0;

pub struct PitchShiftDsp {
    sample_rate: f32,
    semitones: f32,
    window_ms: f32,
    mix: f32,
    buf_l: Vec<f32>,
    buf_r: Vec<f32>,
    write: usize,
    /// Tap sweep phase in [0, 1).
    phase: f32,
}

impl PitchShiftDsp {
    pub fn new(desc: &EffectDesc, sample_rate: f32) -> Self {
        let len = (MAX_WINDOW_MS / 1000.0 * sample_rate) as usize + 2;
        let mut dsp = Self {
            sample_rate,
            semitones: 0.0,
            window_ms: 60.0,
            mix: 1.0,
            buf_l: vec![0.0; len],
            buf_r: vec![0.0; len],
            write: 0,
            phase: 0.0,
        };
        dsp.update(desc);
        dsp
    }

    #[inline]
    fn read(buf: &[f32], write: usize, delay_samples: f32) -> f32 {
        let len = buf.len();
        let d = delay_samples.clamp(0.0, (len - 2) as f32);
        let di = d as usize;
        let frac = d - di as f32;
        let i0 = (write + len - 1 - di) % len;
        let i1 = (write + len - 2 - di) % len;
        buf[i0] * (1.0 - frac) + buf[i1] * frac
    }
}

impl EffectDsp for PitchShiftDsp {
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let ratio = 2f32.powf(self.semitones / 12.0);
        let mix = self.mix.clamp(0.0, 1.0);
        if self.semitones.abs() < 0.001 || mix <= 0.0 {
            // Nothing to do — stay transparent (and keep the line primed).
            for (l, r) in left.iter_mut().zip(right.iter_mut()) {
                self.buf_l[self.write] = *l;
                self.buf_r[self.write] = *r;
                self.write = (self.write + 1) % self.buf_l.len();
            }
            return;
        }
        let window = (self.window_ms.clamp(10.0, MAX_WINDOW_MS) / 1000.0 * self.sample_rate)
            .min((self.buf_l.len() - 2) as f32);
        // Tap delay ramps by (1 - ratio) per sample; phase wraps per window.
        let inc = (1.0 - ratio) / window;

        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            self.buf_l[self.write] = *l;
            self.buf_r[self.write] = *r;
            self.write = (self.write + 1) % self.buf_l.len();

            self.phase = (self.phase + inc).rem_euclid(1.0);
            let p2 = (self.phase + 0.5).rem_euclid(1.0);
            let d1 = self.phase * window;
            let d2 = p2 * window;
            // Equal-power crossfade: each tap fades out exactly as it wraps.
            let g1 = (self.phase * std::f32::consts::PI).sin();
            let g2 = (p2 * std::f32::consts::PI).sin();

            let wl = Self::read(&self.buf_l, self.write, d1) * g1
                + Self::read(&self.buf_l, self.write, d2) * g2;
            let wr = Self::read(&self.buf_r, self.write, d1) * g1
                + Self::read(&self.buf_r, self.write, d2) * g2;

            *l += (wl - *l) * mix;
            *r += (wr - *r) * mix;
        }
    }

    fn update(&mut self, desc: &EffectDesc) {
        if let EffectDesc::PitchShift { semitones, window_ms, mix } = *desc {
            self.semitones = semitones.clamp(-24.0, 24.0);
            self.window_ms = window_ms;
            self.mix = mix;
        }
    }

    fn reset(&mut self) {
        self.buf_l.fill(0.0);
        self.buf_r.fill(0.0);
        self.phase = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Estimate dominant frequency by counting zero crossings.
    fn zero_crossings(signal: &[f32]) -> usize {
        signal.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count()
    }

    #[test]
    fn octave_up_doubles_frequency() {
        let sr = 48_000.0;
        let desc = EffectDesc::PitchShift { semitones: 12.0, window_ms: 60.0, mix: 1.0 };
        let mut dsp = PitchShiftDsp::new(&desc, sr);
        let n = 48_000;
        let hz = 220.0;
        let mut l: Vec<f32> =
            (0..n).map(|i| (std::f32::consts::TAU * hz * i as f32 / sr).sin()).collect();
        let mut r = l.clone();
        dsp.process(&mut l, &mut r);
        // Skip the first window while the line fills.
        let settled = &l[n / 2..];
        let measured = zero_crossings(settled) as f32 / (settled.len() as f32 / sr);
        assert!(
            (measured - 2.0 * hz).abs() < 0.15 * 2.0 * hz,
            "expected ~440 Hz, measured {measured}"
        );
    }
}
