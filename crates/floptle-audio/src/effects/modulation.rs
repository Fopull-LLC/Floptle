//! LFO-modulated effects: chorus and flanger (modulated delay lines) and
//! phaser (LFO-swept allpass cascade).

use super::{EffectDesc, EffectDsp};

/// Longest modulated delay we allocate for: chorus center + depth headroom.
const MAX_MOD_DELAY_MS: f32 = 60.0;

/// Quadrature-friendly sine LFO on a phase in [0, 1).
#[inline]
fn lfo(phase: f32) -> f32 {
    (phase * std::f32::consts::TAU).sin()
}

#[derive(Clone, Copy)]
enum ModKind {
    Chorus,
    Flanger,
}

/// Chorus and flanger share the machinery: a delay line read at an
/// LFO-wobbled position. Chorus = longer center delay, two detuned voices, no
/// feedback. Flanger = short center delay, one voice, feedback.
pub struct ModDelayDsp {
    kind: ModKind,
    sample_rate: f32,
    rate_hz: f32,
    depth_ms: f32,
    feedback: f32,
    mix: f32,
    buf_l: Vec<f32>,
    buf_r: Vec<f32>,
    write: usize,
    phase: f32,
}

impl ModDelayDsp {
    pub fn new(desc: &EffectDesc, sample_rate: f32) -> Self {
        let len = (MAX_MOD_DELAY_MS / 1000.0 * sample_rate) as usize + 2;
        let kind = match desc {
            EffectDesc::Flanger { .. } => ModKind::Flanger,
            _ => ModKind::Chorus,
        };
        let mut dsp = Self {
            kind,
            sample_rate,
            rate_hz: 0.8,
            depth_ms: 4.0,
            feedback: 0.0,
            mix: 0.5,
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
        let d = delay_samples.clamp(1.0, (len - 2) as f32);
        let di = d as usize;
        let frac = d - di as f32;
        let i0 = (write + len - di) % len;
        let i1 = (write + len - di - 1) % len;
        buf[i0] * (1.0 - frac) + buf[i1] * frac
    }
}

impl EffectDsp for ModDelayDsp {
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let ms = self.sample_rate / 1000.0;
        // Center delay: chorus sits well clear of the comb zone; flanger in it.
        let (center, voices) = match self.kind {
            ModKind::Chorus => (18.0 * ms, 2),
            ModKind::Flanger => (3.0 * ms, 1),
        };
        let depth = (self.depth_ms.clamp(0.05, 25.0) * ms).min(center - 1.0);
        let fb = self.feedback.clamp(-0.95, 0.95);
        let mix = self.mix.clamp(0.0, 1.0);
        let inc = self.rate_hz.clamp(0.01, 20.0) / self.sample_rate;

        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let mut wet_l = 0.0;
            let mut wet_r = 0.0;
            for v in 0..voices {
                // Each voice gets a phase offset; L/R get quadrature offsets
                // so the image moves instead of pumping.
                let p = self.phase + v as f32 * 0.37;
                let dl = center + depth * lfo(p);
                let dr = center + depth * lfo(p + 0.25);
                wet_l += Self::read(&self.buf_l, self.write, dl);
                wet_r += Self::read(&self.buf_r, self.write, dr);
            }
            let inv = 1.0 / voices as f32;
            wet_l *= inv;
            wet_r *= inv;

            self.buf_l[self.write] = *l + wet_l * fb;
            self.buf_r[self.write] = *r + wet_r * fb;
            self.write = (self.write + 1) % self.buf_l.len();
            self.phase = (self.phase + inc).fract();

            *l += (wet_l - *l) * mix;
            *r += (wet_r - *r) * mix;
        }
    }

    fn update(&mut self, desc: &EffectDesc) {
        match *desc {
            EffectDesc::Chorus { rate_hz, depth_ms, mix } => {
                self.rate_hz = rate_hz;
                self.depth_ms = depth_ms;
                self.feedback = 0.0;
                self.mix = mix;
            }
            EffectDesc::Flanger { rate_hz, depth_ms, feedback, mix } => {
                self.rate_hz = rate_hz;
                self.depth_ms = depth_ms;
                self.feedback = feedback;
                self.mix = mix;
            }
            _ => {}
        }
    }

    fn reset(&mut self) {
        self.buf_l.fill(0.0);
        self.buf_r.fill(0.0);
        self.phase = 0.0;
    }
}

/// One first-order allpass stage per channel pair.
#[derive(Clone, Copy, Default)]
struct AllpassStage {
    x1: f32,
    y1: f32,
}

impl AllpassStage {
    #[inline]
    fn tick(&mut self, x: f32, a: f32) -> f32 {
        let y = a * x + self.x1 - a * self.y1;
        self.x1 = x;
        self.y1 = y;
        y
    }
}

pub const MAX_PHASER_STAGES: usize = 12;

pub struct PhaserDsp {
    sample_rate: f32,
    rate_hz: f32,
    stages: usize,
    center_hz: f32,
    depth: f32,
    feedback: f32,
    mix: f32,
    stages_l: [AllpassStage; MAX_PHASER_STAGES],
    stages_r: [AllpassStage; MAX_PHASER_STAGES],
    fb_l: f32,
    fb_r: f32,
    phase: f32,
}

impl PhaserDsp {
    pub fn new(desc: &EffectDesc, sample_rate: f32) -> Self {
        let mut dsp = Self {
            sample_rate,
            rate_hz: 0.4,
            stages: 6,
            center_hz: 900.0,
            depth: 0.7,
            feedback: 0.4,
            mix: 0.5,
            stages_l: [AllpassStage::default(); MAX_PHASER_STAGES],
            stages_r: [AllpassStage::default(); MAX_PHASER_STAGES],
            fb_l: 0.0,
            fb_r: 0.0,
            phase: 0.0,
        };
        dsp.update(desc);
        dsp
    }
}

impl EffectDsp for PhaserDsp {
    fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let inc = self.rate_hz.clamp(0.01, 10.0) / self.sample_rate;
        let mix = self.mix.clamp(0.0, 1.0);
        let fb = self.feedback.clamp(-0.95, 0.95);
        let n = self.stages.clamp(2, MAX_PHASER_STAGES);

        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            // Sweep the allpass corner geometrically around center_hz.
            let sweep = 2f32.powf(lfo(self.phase) * 2.0 * self.depth.clamp(0.0, 1.0));
            let fc = (self.center_hz * sweep).clamp(40.0, self.sample_rate * 0.45);
            let t = (std::f32::consts::PI * fc / self.sample_rate).tan();
            let a = (t - 1.0) / (t + 1.0);
            self.phase = (self.phase + inc).fract();

            let mut wl = *l + self.fb_l * fb;
            let mut wr = *r + self.fb_r * fb;
            for i in 0..n {
                wl = self.stages_l[i].tick(wl, a);
                wr = self.stages_r[i].tick(wr, a);
            }
            self.fb_l = wl;
            self.fb_r = wr;

            *l += (wl - *l) * mix;
            *r += (wr - *r) * mix;
        }
    }

    fn update(&mut self, desc: &EffectDesc) {
        let EffectDesc::Phaser { rate_hz, stages, center_hz, depth, feedback, mix } = *desc else {
            return;
        };
        self.rate_hz = rate_hz;
        self.stages = stages.clamp(2, MAX_PHASER_STAGES as u32) as usize;
        self.center_hz = center_hz;
        self.depth = depth;
        self.feedback = feedback;
        self.mix = mix;
    }

    fn reset(&mut self) {
        self.stages_l = [AllpassStage::default(); MAX_PHASER_STAGES];
        self.stages_r = [AllpassStage::default(); MAX_PHASER_STAGES];
        self.fb_l = 0.0;
        self.fb_r = 0.0;
        self.phase = 0.0;
    }
}
