//! Parametric EQ: a chain of RBJ-cookbook biquad bands.
//!
//! The same coefficient math backs both the audio path and the editor's EQ
//! graph — [`EqBand::response_db`] evaluates a band's magnitude response at an
//! arbitrary frequency so the curve drawn in the mixer panel is exactly what
//! the filter does.

use serde::{Deserialize, Serialize};

use super::{EffectDesc, EffectDsp};

/// Filter shape of one EQ band.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EqBandKind {
    LowShelf,
    Peak,
    HighShelf,
    LowPass,
    HighPass,
    Notch,
}

/// One band of the parametric EQ.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct EqBand {
    pub kind: EqBandKind,
    /// Center / corner frequency in Hz.
    pub freq_hz: f32,
    /// Boost/cut in dB (ignored by LowPass/HighPass/Notch).
    pub gain_db: f32,
    /// Bandwidth: higher = narrower. ~0.7 is a broad musical band.
    pub q: f32,
    pub enabled: bool,
}

impl EqBand {
    /// The starting 4-band layout new EQs get: low shelf, two peaks, high shelf.
    pub fn default_bands() -> Vec<EqBand> {
        vec![
            EqBand { kind: EqBandKind::LowShelf, freq_hz: 120.0, gain_db: 0.0, q: 0.71, enabled: true },
            EqBand { kind: EqBandKind::Peak, freq_hz: 500.0, gain_db: 0.0, q: 1.0, enabled: true },
            EqBand { kind: EqBandKind::Peak, freq_hz: 2_500.0, gain_db: 0.0, q: 1.0, enabled: true },
            EqBand { kind: EqBandKind::HighShelf, freq_hz: 8_000.0, gain_db: 0.0, q: 0.71, enabled: true },
        ]
    }

    /// RBJ biquad coefficients for this band, normalized (a0 divided out).
    /// Returns (b0, b1, b2, a1, a2).
    pub fn coefficients(&self, sample_rate: f32) -> (f32, f32, f32, f32, f32) {
        let freq = self.freq_hz.clamp(10.0, sample_rate * 0.49);
        let q = self.q.max(0.05);
        let w0 = std::f32::consts::TAU * freq / sample_rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * q);
        let a = 10f32.powf(self.gain_db / 40.0); // sqrt of linear gain

        let (b0, b1, b2, a0, a1, a2) = match self.kind {
            EqBandKind::Peak => (
                1.0 + alpha * a,
                -2.0 * cos,
                1.0 - alpha * a,
                1.0 + alpha / a,
                -2.0 * cos,
                1.0 - alpha / a,
            ),
            EqBandKind::LowShelf => {
                let s = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) - (a - 1.0) * cos + s),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
                    a * ((a + 1.0) - (a - 1.0) * cos - s),
                    (a + 1.0) + (a - 1.0) * cos + s,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cos),
                    (a + 1.0) + (a - 1.0) * cos - s,
                )
            }
            EqBandKind::HighShelf => {
                let s = 2.0 * a.sqrt() * alpha;
                (
                    a * ((a + 1.0) + (a - 1.0) * cos + s),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
                    a * ((a + 1.0) + (a - 1.0) * cos - s),
                    (a + 1.0) - (a - 1.0) * cos + s,
                    2.0 * ((a - 1.0) - (a + 1.0) * cos),
                    (a + 1.0) - (a - 1.0) * cos - s,
                )
            }
            EqBandKind::LowPass => (
                (1.0 - cos) * 0.5,
                1.0 - cos,
                (1.0 - cos) * 0.5,
                1.0 + alpha,
                -2.0 * cos,
                1.0 - alpha,
            ),
            EqBandKind::HighPass => (
                (1.0 + cos) * 0.5,
                -(1.0 + cos),
                (1.0 + cos) * 0.5,
                1.0 + alpha,
                -2.0 * cos,
                1.0 - alpha,
            ),
            EqBandKind::Notch => (1.0, -2.0 * cos, 1.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha),
        };
        (b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0)
    }

    /// Magnitude response of this band at `freq_hz`, in dB. Used by the EQ
    /// graph editor; disabled bands contribute 0 dB.
    pub fn response_db(&self, freq_hz: f32, sample_rate: f32) -> f32 {
        if !self.enabled {
            return 0.0;
        }
        let (b0, b1, b2, a1, a2) = self.coefficients(sample_rate);
        // |H(e^jw)|^2 evaluated directly from the difference equation.
        let w = std::f32::consts::TAU * freq_hz / sample_rate;
        let (c1, c2) = (w.cos(), (2.0 * w).cos());
        let (s1, s2) = (w.sin(), (2.0 * w).sin());
        let num_re = b0 + b1 * c1 + b2 * c2;
        let num_im = -(b1 * s1 + b2 * s2);
        let den_re = 1.0 + a1 * c1 + a2 * c2;
        let den_im = -(a1 * s1 + a2 * s2);
        let num = num_re * num_re + num_im * num_im;
        let den = (den_re * den_re + den_im * den_im).max(1e-12);
        10.0 * (num / den).max(1e-12).log10()
    }
}

/// Transposed direct-form-II biquad section.
#[derive(Clone, Copy, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn set(&mut self, c: (f32, f32, f32, f32, f32)) {
        (self.b0, self.b1, self.b2, self.a1, self.a2) = c;
    }

    #[inline]
    fn tick(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

pub struct ParametricEqDsp {
    sample_rate: f32,
    /// One section per enabled band, per channel.
    left: Vec<Biquad>,
    right: Vec<Biquad>,
}

impl ParametricEqDsp {
    pub fn new(desc: &EffectDesc, sample_rate: f32) -> Self {
        let mut dsp = Self { sample_rate, left: Vec::new(), right: Vec::new() };
        dsp.update(desc);
        dsp
    }
}

impl EffectDsp for ParametricEqDsp {
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        for bq in &mut self.left {
            for s in left.iter_mut() {
                *s = bq.tick(*s);
            }
        }
        for bq in &mut self.right {
            for s in right.iter_mut() {
                *s = bq.tick(*s);
            }
        }
    }

    fn update(&mut self, desc: &EffectDesc) {
        let EffectDesc::ParametricEq { bands } = desc else { return };
        let enabled: Vec<_> = bands.iter().filter(|b| b.enabled).collect();
        // Keep filter state where the section count matches so live edits
        // don't zipper; resize only when bands are added/removed.
        self.left.resize(enabled.len(), Biquad::default());
        self.right.resize(enabled.len(), Biquad::default());
        for (i, band) in enabled.iter().enumerate() {
            let c = band.coefficients(self.sample_rate);
            self.left[i].set(c);
            self.right[i].set(c);
        }
    }

    fn reset(&mut self) {
        for bq in self.left.iter_mut().chain(self.right.iter_mut()) {
            bq.z1 = 0.0;
            bq.z2 = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_bands_pass_through() {
        let desc = EffectDesc::ParametricEq { bands: EqBand::default_bands() };
        let mut dsp = desc.build(48_000.0);
        let src: Vec<f32> = (0..256).map(|i| ((i as f32) * 0.1).sin() * 0.5).collect();
        let mut l = src.clone();
        let mut r = src.clone();
        dsp.process(&mut l, &mut r);
        // 0 dB everywhere -> output ~= input (after the filters settle).
        for (a, b) in src.iter().zip(l.iter()).skip(64) {
            assert!((a - b).abs() < 1e-3, "flat EQ altered signal: {a} vs {b}");
        }
    }

    #[test]
    fn peak_boost_raises_response_at_center() {
        let band = EqBand { kind: EqBandKind::Peak, freq_hz: 1000.0, gain_db: 6.0, q: 1.0, enabled: true };
        let at_center = band.response_db(1000.0, 48_000.0);
        let far_away = band.response_db(60.0, 48_000.0);
        assert!((at_center - 6.0).abs() < 0.2, "center response {at_center} != 6 dB");
        assert!(far_away.abs() < 0.5, "far response {far_away} should be ~0 dB");
    }
}
