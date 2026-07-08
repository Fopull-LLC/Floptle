//! Waveshaping distortion + the gain/width utility.

use super::{db_to_lin, EffectDesc, EffectDsp};

pub struct DistortionDsp {
    sample_rate: f32,
    drive: f32,
    tone: f32,
    mix: f32,
    /// Post-shaper one-pole lowpass state (the tone control).
    lp_l: f32,
    lp_r: f32,
}

impl DistortionDsp {
    pub fn new(desc: &EffectDesc, sample_rate: f32) -> Self {
        let mut dsp = Self { sample_rate, drive: 0.4, tone: 0.7, mix: 1.0, lp_l: 0.0, lp_r: 0.0 };
        dsp.update(desc);
        dsp
    }

    #[inline]
    fn shape(x: f32, gain: f32) -> f32 {
        // tanh soft clip, normalized so unity input stays near unity.
        (x * gain).tanh() / gain.tanh().max(1e-3)
    }
}

impl EffectDsp for DistortionDsp {
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let gain = 1.0 + self.drive.clamp(0.0, 1.0) * 24.0;
        let mix = self.mix.clamp(0.0, 1.0);
        // tone 0..1 -> lowpass corner 400 Hz .. wide open.
        let fc = 400.0 * (self.sample_rate * 0.45 / 400.0).powf(self.tone.clamp(0.0, 1.0));
        let k = 1.0 - (-std::f32::consts::TAU * fc / self.sample_rate).exp();

        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let dl = Self::shape(*l, gain);
            let dr = Self::shape(*r, gain);
            self.lp_l += (dl - self.lp_l) * k;
            self.lp_r += (dr - self.lp_r) * k;
            *l += (self.lp_l - *l) * mix;
            *r += (self.lp_r - *r) * mix;
        }
    }

    fn update(&mut self, desc: &EffectDesc) {
        if let EffectDesc::Distortion { drive, tone, mix } = *desc {
            self.drive = drive;
            self.tone = tone;
            self.mix = mix;
        }
    }

    fn reset(&mut self) {
        self.lp_l = 0.0;
        self.lp_r = 0.0;
    }
}

pub struct UtilityDsp {
    gain: f32,
    width: f32,
}

impl UtilityDsp {
    pub fn new(desc: &EffectDesc) -> Self {
        let mut dsp = Self { gain: 1.0, width: 1.0 };
        dsp.update(desc);
        dsp
    }
}

impl EffectDsp for UtilityDsp {
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let w = self.width.clamp(0.0, 2.0);
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let mid = (*l + *r) * 0.5;
            let side = (*l - *r) * 0.5 * w;
            *l = (mid + side) * self.gain;
            *r = (mid - side) * self.gain;
        }
    }

    fn update(&mut self, desc: &EffectDesc) {
        if let EffectDesc::Utility { gain_db, width } = *desc {
            self.gain = db_to_lin(gain_db);
            self.width = width;
        }
    }

    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utility_width_zero_makes_mono() {
        let desc = EffectDesc::Utility { gain_db: 0.0, width: 0.0 };
        let mut dsp = UtilityDsp::new(&desc);
        let mut l = vec![1.0f32; 16];
        let mut r = vec![-1.0f32; 16];
        dsp.process(&mut l, &mut r);
        for (a, b) in l.iter().zip(r.iter()) {
            assert!((a - b).abs() < 1e-6, "width 0 should collapse to mono");
        }
    }

    #[test]
    fn distortion_clips_hot_signal() {
        let desc = EffectDesc::Distortion { drive: 1.0, tone: 1.0, mix: 1.0 };
        let mut dsp = DistortionDsp::new(&desc, 48_000.0);
        let mut l = vec![2.0f32; 64];
        let mut r = vec![2.0f32; 64];
        dsp.process(&mut l, &mut r);
        assert!(l.iter().all(|s| s.abs() < 1.3), "shaper failed to clamp");
    }
}
