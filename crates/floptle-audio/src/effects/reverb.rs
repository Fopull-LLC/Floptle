//! Freeverb-style stereo reverb: 8 parallel damped combs + 4 series allpasses
//! per channel, right channel detuned by a fixed sample offset for width.

use super::{EffectDesc, EffectDsp};

/// Classic Freeverb tunings (samples at 44.1 kHz; scaled to the device rate).
const COMB_TUNING: [usize; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASS_TUNING: [usize; 4] = [556, 441, 341, 225];
const STEREO_SPREAD: usize = 23;
const MAX_PRE_DELAY_MS: f32 = 250.0;

struct Comb {
    buf: Vec<f32>,
    idx: usize,
    feedback: f32,
    damp: f32,
    filter: f32,
}

impl Comb {
    fn new(len: usize) -> Self {
        Self { buf: vec![0.0; len.max(1)], idx: 0, feedback: 0.8, damp: 0.2, filter: 0.0 }
    }

    #[inline]
    fn tick(&mut self, x: f32) -> f32 {
        let out = self.buf[self.idx];
        self.filter = out * (1.0 - self.damp) + self.filter * self.damp;
        self.buf[self.idx] = x + self.filter * self.feedback;
        self.idx = (self.idx + 1) % self.buf.len();
        out
    }
}

struct Allpass {
    buf: Vec<f32>,
    idx: usize,
}

impl Allpass {
    fn new(len: usize) -> Self {
        Self { buf: vec![0.0; len.max(1)], idx: 0 }
    }

    #[inline]
    fn tick(&mut self, x: f32) -> f32 {
        const G: f32 = 0.5;
        let delayed = self.buf[self.idx];
        let out = delayed - x;
        self.buf[self.idx] = x + delayed * G;
        self.idx = (self.idx + 1) % self.buf.len();
        out
    }
}

pub struct ReverbDsp {
    combs_l: Vec<Comb>,
    combs_r: Vec<Comb>,
    allpasses_l: Vec<Allpass>,
    allpasses_r: Vec<Allpass>,
    pre_l: Vec<f32>,
    pre_r: Vec<f32>,
    pre_idx: usize,
    sample_rate: f32,
    pre_delay_ms: f32,
    width: f32,
    mix: f32,
}

impl ReverbDsp {
    pub fn new(desc: &EffectDesc, sample_rate: f32) -> Self {
        let scale = sample_rate / 44_100.0;
        let sized = |n: usize| ((n as f32 * scale) as usize).max(1);
        let pre_len = (MAX_PRE_DELAY_MS / 1000.0 * sample_rate) as usize + 1;
        let mut dsp = Self {
            combs_l: COMB_TUNING.iter().map(|&n| Comb::new(sized(n))).collect(),
            combs_r: COMB_TUNING.iter().map(|&n| Comb::new(sized(n + STEREO_SPREAD))).collect(),
            allpasses_l: ALLPASS_TUNING.iter().map(|&n| Allpass::new(sized(n))).collect(),
            allpasses_r: ALLPASS_TUNING.iter().map(|&n| Allpass::new(sized(n + STEREO_SPREAD))).collect(),
            pre_l: vec![0.0; pre_len],
            pre_r: vec![0.0; pre_len],
            pre_idx: 0,
            sample_rate,
            pre_delay_ms: 12.0,
            width: 1.0,
            mix: 0.25,
        };
        dsp.update(desc);
        dsp
    }
}

impl EffectDsp for ReverbDsp {
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let mix = self.mix.clamp(0.0, 1.0);
        let width = self.width.clamp(0.0, 1.0);
        let wet1 = width * 0.5 + 0.5;
        let wet2 = (1.0 - width) * 0.5;
        let pre = ((self.pre_delay_ms.clamp(0.0, MAX_PRE_DELAY_MS) / 1000.0 * self.sample_rate)
            as usize)
            .min(self.pre_l.len() - 1);

        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            self.pre_l[self.pre_idx] = *l;
            self.pre_r[self.pre_idx] = *r;
            let read = (self.pre_idx + self.pre_l.len() - pre) % self.pre_l.len();
            self.pre_idx = (self.pre_idx + 1) % self.pre_l.len();

            // Mono-summed tank input at Freeverb's fixed input gain.
            let input = (self.pre_l[read] + self.pre_r[read]) * 0.015;
            let mut out_l = 0.0;
            let mut out_r = 0.0;
            for c in &mut self.combs_l {
                out_l += c.tick(input);
            }
            for c in &mut self.combs_r {
                out_r += c.tick(input);
            }
            for a in &mut self.allpasses_l {
                out_l = a.tick(out_l);
            }
            for a in &mut self.allpasses_r {
                out_r = a.tick(out_r);
            }
            let wet_l = out_l * wet1 + out_r * wet2;
            let wet_r = out_r * wet1 + out_l * wet2;
            *l += (wet_l - *l) * mix;
            *r += (wet_r - *r) * mix;
        }
    }

    fn update(&mut self, desc: &EffectDesc) {
        let EffectDesc::Reverb { room_size, damping, width, mix, pre_delay_ms } = *desc else {
            return;
        };
        let feedback = 0.7 + room_size.clamp(0.0, 1.0) * 0.28;
        let damp = damping.clamp(0.0, 1.0) * 0.9;
        for c in self.combs_l.iter_mut().chain(self.combs_r.iter_mut()) {
            c.feedback = feedback;
            c.damp = damp;
        }
        self.width = width;
        self.mix = mix;
        self.pre_delay_ms = pre_delay_ms;
    }

    fn reset(&mut self) {
        for c in self.combs_l.iter_mut().chain(self.combs_r.iter_mut()) {
            c.buf.fill(0.0);
            c.filter = 0.0;
        }
        for a in self.allpasses_l.iter_mut().chain(self.allpasses_r.iter_mut()) {
            a.buf.fill(0.0);
        }
        self.pre_l.fill(0.0);
        self.pre_r.fill(0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn impulse_produces_decaying_tail() {
        let desc = EffectDesc::Reverb { room_size: 0.7, damping: 0.3, width: 1.0, mix: 1.0, pre_delay_ms: 0.0 };
        let mut dsp = ReverbDsp::new(&desc, 48_000.0);
        let n = 48_000; // 1 s
        let mut l = vec![0.0f32; n];
        let mut r = vec![0.0f32; n];
        l[0] = 1.0;
        r[0] = 1.0;
        dsp.process(&mut l, &mut r);
        let early: f32 = l[2_000..10_000].iter().map(|s| s * s).sum();
        let late: f32 = l[38_000..46_000].iter().map(|s| s * s).sum();
        assert!(early > 1e-6, "no reverb tail at all");
        assert!(late < early, "tail did not decay: early {early} late {late}");
        assert!(l.iter().all(|s| s.is_finite() && s.abs() < 10.0), "unstable tank");
    }
}
