//! Dynamics: feed-forward compressor and lookahead-free limiter. One
//! processor handles both — a limiter is a compressor with an infinite ratio,
//! an instant attack, and the threshold spelled as a ceiling.

use super::{db_to_lin, lin_to_db, EffectDesc, EffectDsp};

#[derive(Clone, Copy)]
enum DynKind {
    Compressor,
    Limiter,
}

pub struct DynamicsDsp {
    kind: DynKind,
    sample_rate: f32,
    threshold_db: f32,
    ratio: f32,
    attack_ms: f32,
    release_ms: f32,
    makeup: f32,
    /// Envelope of the stereo-linked level, linear.
    env: f32,
}

impl DynamicsDsp {
    pub fn new(desc: &EffectDesc, sample_rate: f32) -> Self {
        let kind = match desc {
            EffectDesc::Limiter { .. } => DynKind::Limiter,
            _ => DynKind::Compressor,
        };
        let mut dsp = Self {
            kind,
            sample_rate,
            threshold_db: -18.0,
            ratio: 3.0,
            attack_ms: 10.0,
            release_ms: 120.0,
            makeup: 1.0,
            env: 0.0,
        };
        dsp.update(desc);
        dsp
    }

    #[inline]
    fn coef(ms: f32, sample_rate: f32) -> f32 {
        // One-pole time constant; ~63% of the way there after `ms`.
        (-1.0 / (ms.max(0.01) / 1000.0 * sample_rate)).exp()
    }
}

impl EffectDsp for DynamicsDsp {
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let attack = match self.kind {
            DynKind::Compressor => Self::coef(self.attack_ms, self.sample_rate),
            DynKind::Limiter => 0.0, // instant clamp — it's a ceiling
        };
        let release = Self::coef(self.release_ms, self.sample_rate);
        let thr_db = self.threshold_db;
        let inv_ratio = match self.kind {
            DynKind::Compressor => 1.0 / self.ratio.max(1.0),
            DynKind::Limiter => 0.0,
        };

        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let level = l.abs().max(r.abs());
            let coef = if level > self.env { attack } else { release };
            self.env = level + coef * (self.env - level);

            let env_db = lin_to_db(self.env);
            let over = env_db - thr_db;
            let gain = if over > 0.0 {
                // Gain that maps `over` dB above threshold to `over/ratio`.
                db_to_lin(over * (inv_ratio - 1.0))
            } else {
                1.0
            };
            *l *= gain * self.makeup;
            *r *= gain * self.makeup;
        }
    }

    fn update(&mut self, desc: &EffectDesc) {
        match *desc {
            EffectDesc::Compressor { threshold_db, ratio, attack_ms, release_ms, makeup_db } => {
                self.threshold_db = threshold_db;
                self.ratio = ratio;
                self.attack_ms = attack_ms;
                self.release_ms = release_ms;
                self.makeup = db_to_lin(makeup_db);
            }
            EffectDesc::Limiter { ceiling_db, release_ms } => {
                self.threshold_db = ceiling_db;
                self.release_ms = release_ms;
                self.makeup = 1.0;
            }
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.env = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressor_reduces_loud_signal() {
        let desc = EffectDesc::Compressor {
            threshold_db: -20.0,
            ratio: 4.0,
            attack_ms: 1.0,
            release_ms: 100.0,
            makeup_db: 0.0,
        };
        let mut dsp = DynamicsDsp::new(&desc, 48_000.0);
        let mut l = vec![0.8f32; 4800];
        let mut r = vec![0.8f32; 4800];
        dsp.process(&mut l, &mut r);
        // 0.8 is ~ -1.9 dBFS: 18 dB over threshold -> ~13.5 dB reduction.
        let settled = l[4000];
        assert!(settled < 0.3, "compressor barely compressed: {settled}");
        assert!(settled > 0.05, "compressor over-compressed: {settled}");
    }

    #[test]
    fn limiter_holds_ceiling() {
        let desc = EffectDesc::Limiter { ceiling_db: -6.0, release_ms: 50.0 };
        let mut dsp = DynamicsDsp::new(&desc, 48_000.0);
        let mut l = vec![1.0f32; 4800];
        let mut r = vec![1.0f32; 4800];
        dsp.process(&mut l, &mut r);
        let ceiling = db_to_lin(-6.0);
        assert!(
            l[100..].iter().all(|s| *s <= ceiling * 1.02),
            "limiter let signal over the ceiling"
        );
    }
}
