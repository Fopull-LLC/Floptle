//! Stereo feedback delay with in-loop damping and optional ping-pong.

use serde::{Deserialize, Serialize};

use super::{EffectDesc, EffectDsp};

/// Maximum delay time the line allocates for (params clamp to this).
pub const MAX_DELAY_MS: f32 = 4_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct Params {
    time_ms: f32,
    feedback: f32,
    mix: f32,
    ping_pong: bool,
    damping: f32,
}

pub struct DelayDsp {
    sample_rate: f32,
    p: Params,
    buf_l: Vec<f32>,
    buf_r: Vec<f32>,
    write: usize,
    /// One-pole lowpass state inside each feedback path.
    damp_l: f32,
    damp_r: f32,
}

impl DelayDsp {
    pub fn new(desc: &EffectDesc, sample_rate: f32) -> Self {
        let len = (MAX_DELAY_MS / 1000.0 * sample_rate) as usize + 2;
        let mut dsp = Self {
            sample_rate,
            p: Params { time_ms: 350.0, feedback: 0.35, mix: 0.25, ping_pong: false, damping: 0.3 },
            buf_l: vec![0.0; len],
            buf_r: vec![0.0; len],
            write: 0,
            damp_l: 0.0,
            damp_r: 0.0,
        };
        dsp.update(desc);
        dsp
    }

    #[inline]
    fn read(buf: &[f32], write: usize, delay_samples: f32) -> f32 {
        // Linear interpolation behind the write head.
        let len = buf.len();
        let d = delay_samples.clamp(1.0, (len - 2) as f32);
        let di = d as usize;
        let frac = d - di as f32;
        let i0 = (write + len - di) % len;
        let i1 = (write + len - di - 1) % len;
        buf[i0] * (1.0 - frac) + buf[i1] * frac
    }
}

impl EffectDsp for DelayDsp {
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let delay = (self.p.time_ms.clamp(1.0, MAX_DELAY_MS) / 1000.0 * self.sample_rate).max(1.0);
        let fb = self.p.feedback.clamp(0.0, 0.98);
        let mix = self.p.mix.clamp(0.0, 1.0);
        // damping 0..1 -> one-pole coefficient; higher = darker repeats.
        let dcoef = self.p.damping.clamp(0.0, 0.99);

        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let tap_l = Self::read(&self.buf_l, self.write, delay);
            let tap_r = Self::read(&self.buf_r, self.write, delay);
            self.damp_l += (tap_l - self.damp_l) * (1.0 - dcoef);
            self.damp_r += (tap_r - self.damp_r) * (1.0 - dcoef);

            if self.p.ping_pong {
                // Echoes cross channels each repeat.
                self.buf_l[self.write] = *r + self.damp_r * fb;
                self.buf_r[self.write] = *l + self.damp_l * fb;
            } else {
                self.buf_l[self.write] = *l + self.damp_l * fb;
                self.buf_r[self.write] = *r + self.damp_r * fb;
            }
            self.write = (self.write + 1) % self.buf_l.len();

            *l += (tap_l - *l) * mix;
            *r += (tap_r - *r) * mix;
        }
    }

    fn update(&mut self, desc: &EffectDesc) {
        if let EffectDesc::Delay { time_ms, feedback, mix, ping_pong, damping } = *desc {
            self.p = Params { time_ms, feedback, mix, ping_pong, damping };
        }
    }

    fn reset(&mut self) {
        self.buf_l.fill(0.0);
        self.buf_r.fill(0.0);
        self.damp_l = 0.0;
        self.damp_r = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn impulse_returns_after_delay_time() {
        let sr = 48_000.0;
        let desc = EffectDesc::Delay { time_ms: 10.0, feedback: 0.0, mix: 1.0, ping_pong: false, damping: 0.0 };
        let mut dsp = DelayDsp::new(&desc, sr);
        let n = (sr * 0.02) as usize; // 20 ms
        let mut l = vec![0.0f32; n];
        let mut r = vec![0.0f32; n];
        l[0] = 1.0;
        r[0] = 1.0;
        dsp.process(&mut l, &mut r);
        let expect = (sr * 0.010) as usize;
        let peak = l.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).unwrap().0;
        assert!(
            (peak as i64 - expect as i64).unsigned_abs() <= 2,
            "echo at {peak}, expected ~{expect}"
        );
    }
}
