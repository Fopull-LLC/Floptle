//! Mixer-track effects: serializable descriptors + real-time DSP.
//!
//! Each effect is two halves:
//! - [`EffectDesc`] — a plain serde struct of knobs. This is what scenes,
//!   the mixer panel, and Lua read/write. Cheap to clone and diff.
//! - a DSP processor (one per variant) built from the desc at the engine's
//!   sample rate. Processors implement [`EffectDsp`] and run on the audio
//!   thread only.
//!
//! Live tweaking: the engine sends the updated desc to the audio thread and
//! the processor absorbs it via [`EffectDsp::update`] *without* clearing its
//! delay lines / filter state, so dragging a knob never clicks or cuts tails.

mod delay;
mod distortion;
mod dynamics;
mod eq;
mod modulation;
mod pitch;
mod reverb;

pub use eq::{EqBand, EqBandKind};

use serde::{Deserialize, Serialize};

/// Convert decibels to linear gain.
#[inline]
pub fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Convert linear gain to decibels (floored at -120 dB).
#[inline]
pub fn lin_to_db(lin: f32) -> f32 {
    20.0 * lin.max(1e-6).log10()
}

/// One effect's full parameter set. The tag doubles as the user-facing name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EffectDesc {
    /// Parametric EQ: an ordered set of filter bands (shelves, peaks, cuts).
    ParametricEq {
        bands: Vec<EqBand>,
    },
    /// Stereo feedback delay with damping and optional ping-pong.
    Delay {
        /// Delay time in milliseconds.
        time_ms: f32,
        /// 0..1 — how much of the delayed signal feeds back.
        feedback: f32,
        /// 0..1 — wet/dry mix.
        mix: f32,
        /// Bounce the echo between left and right.
        ping_pong: bool,
        /// 0..1 — high-frequency damping applied inside the feedback loop.
        damping: f32,
    },
    /// Freeverb-style stereo reverb.
    Reverb {
        /// 0..1 — perceived room size (comb feedback).
        room_size: f32,
        /// 0..1 — high-frequency damping inside the tank.
        damping: f32,
        /// 0..1 — stereo width of the wet signal.
        width: f32,
        /// 0..1 — wet/dry mix.
        mix: f32,
        /// Pre-delay before the tank, in milliseconds.
        pre_delay_ms: f32,
    },
    /// Multi-voice modulated-delay chorus.
    Chorus {
        /// LFO rate in Hz.
        rate_hz: f32,
        /// Modulation depth in milliseconds.
        depth_ms: f32,
        /// 0..1 — wet/dry mix.
        mix: f32,
    },
    /// Short modulated delay with feedback (jet-engine sweep).
    Flanger {
        /// LFO rate in Hz.
        rate_hz: f32,
        /// Modulation depth in milliseconds.
        depth_ms: f32,
        /// -0.95..0.95 — feedback around the delay (negative inverts).
        feedback: f32,
        /// 0..1 — wet/dry mix.
        mix: f32,
    },
    /// Cascaded-allpass phaser swept by an LFO.
    Phaser {
        /// LFO rate in Hz.
        rate_hz: f32,
        /// Number of allpass stages (2..=12, even numbers sound classic).
        stages: u32,
        /// Sweep center frequency in Hz.
        center_hz: f32,
        /// 0..1 — sweep depth around the center.
        depth: f32,
        /// -0.95..0.95 — feedback around the allpass chain.
        feedback: f32,
        /// 0..1 — wet/dry mix.
        mix: f32,
    },
    /// Time-preserving pitch shift (dual-tap granular).
    PitchShift {
        /// Shift in semitones, negative = down. Fractional values work.
        semitones: f32,
        /// Grain window in milliseconds — smaller = tighter but warblier.
        window_ms: f32,
        /// 0..1 — wet/dry mix (1 = fully shifted).
        mix: f32,
    },
    /// Feed-forward compressor.
    Compressor {
        /// Level where compression starts, in dBFS.
        threshold_db: f32,
        /// Compression ratio (2 = 2:1). 1 = off.
        ratio: f32,
        /// Attack time in milliseconds.
        attack_ms: f32,
        /// Release time in milliseconds.
        release_ms: f32,
        /// Post gain in dB.
        makeup_db: f32,
    },
    /// Hard ceiling with fast gain riding — put on master to stop clipping.
    Limiter {
        /// Output ceiling in dBFS.
        ceiling_db: f32,
        /// Release time in milliseconds.
        release_ms: f32,
    },
    /// Waveshaping distortion with tone control.
    Distortion {
        /// 0..1 — drive amount.
        drive: f32,
        /// 0..1 — post lowpass tone (1 = open).
        tone: f32,
        /// 0..1 — wet/dry mix.
        mix: f32,
    },
    /// Gain / stereo-width utility.
    Utility {
        /// Gain in dB.
        gain_db: f32,
        /// Stereo width: 0 = mono, 1 = unchanged, 2 = extra wide.
        width: f32,
    },
}

impl EffectDesc {
    /// User-facing display name of this effect type.
    pub fn name(&self) -> &'static str {
        match self {
            Self::ParametricEq { .. } => "Parametric EQ",
            Self::Delay { .. } => "Delay",
            Self::Reverb { .. } => "Reverb",
            Self::Chorus { .. } => "Chorus",
            Self::Flanger { .. } => "Flanger",
            Self::Phaser { .. } => "Phaser",
            Self::PitchShift { .. } => "Pitch Shift",
            Self::Compressor { .. } => "Compressor",
            Self::Limiter { .. } => "Limiter",
            Self::Distortion { .. } => "Distortion",
            Self::Utility { .. } => "Utility",
        }
    }

    /// Every effect type with sensible starting values, for "add effect" menus.
    pub fn all_defaults() -> Vec<EffectDesc> {
        vec![
            Self::ParametricEq { bands: EqBand::default_bands() },
            Self::Delay { time_ms: 350.0, feedback: 0.35, mix: 0.25, ping_pong: false, damping: 0.3 },
            Self::Reverb { room_size: 0.6, damping: 0.4, width: 1.0, mix: 0.25, pre_delay_ms: 12.0 },
            Self::Chorus { rate_hz: 0.8, depth_ms: 4.0, mix: 0.5 },
            Self::Flanger { rate_hz: 0.25, depth_ms: 2.5, feedback: 0.5, mix: 0.5 },
            Self::Phaser { rate_hz: 0.4, stages: 6, center_hz: 900.0, depth: 0.7, feedback: 0.4, mix: 0.5 },
            Self::PitchShift { semitones: 0.0, window_ms: 60.0, mix: 1.0 },
            Self::Compressor { threshold_db: -18.0, ratio: 3.0, attack_ms: 10.0, release_ms: 120.0, makeup_db: 0.0 },
            Self::Limiter { ceiling_db: -0.3, release_ms: 80.0 },
            Self::Distortion { drive: 0.4, tone: 0.7, mix: 1.0 },
            Self::Utility { gain_db: 0.0, width: 1.0 },
        ]
    }

    /// Build the real-time processor for this desc at the given sample rate.
    pub fn build(&self, sample_rate: f32) -> Box<dyn EffectDsp> {
        match self {
            Self::ParametricEq { .. } => Box::new(eq::ParametricEqDsp::new(self, sample_rate)),
            Self::Delay { .. } => Box::new(delay::DelayDsp::new(self, sample_rate)),
            Self::Reverb { .. } => Box::new(reverb::ReverbDsp::new(self, sample_rate)),
            Self::Chorus { .. } | Self::Flanger { .. } => {
                Box::new(modulation::ModDelayDsp::new(self, sample_rate))
            }
            Self::Phaser { .. } => Box::new(modulation::PhaserDsp::new(self, sample_rate)),
            Self::PitchShift { .. } => Box::new(pitch::PitchShiftDsp::new(self, sample_rate)),
            Self::Compressor { .. } | Self::Limiter { .. } => {
                Box::new(dynamics::DynamicsDsp::new(self, sample_rate))
            }
            Self::Distortion { .. } => Box::new(distortion::DistortionDsp::new(self, sample_rate)),
            Self::Utility { .. } => Box::new(distortion::UtilityDsp::new(self)),
        }
    }

    /// True if `other` is the same effect *type* (params may differ) — an
    /// existing processor can absorb it via [`EffectDsp::update`].
    pub fn same_kind(&self, other: &EffectDesc) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
            // Chorus/Flanger and Compressor/Limiter share processors but have
            // different state shapes; treat them as distinct kinds anyway.
            && self.name() == other.name()
    }
}

/// A real-time effect processor. Runs on the audio thread: `process` must not
/// allocate, lock, or block.
pub trait EffectDsp: Send {
    /// Process one stereo block in place.
    fn process(&mut self, left: &mut [f32], right: &mut [f32]);

    /// Absorb new parameters without resetting audible state (tails keep
    /// ringing while knobs move). Only called with a desc of the same kind.
    fn update(&mut self, desc: &EffectDesc);

    /// Clear all internal state (playback stopped / engine rewound).
    fn reset(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desc_ron_roundtrip() {
        for d in EffectDesc::all_defaults() {
            let s = ron::ser::to_string(&d).unwrap();
            let back: EffectDesc = ron::de::from_str(&s).unwrap();
            assert_eq!(d, back, "roundtrip failed for {}", d.name());
        }
    }

    #[test]
    fn build_and_process_all() {
        // Every default effect must run a block without panicking and produce
        // finite output.
        for d in EffectDesc::all_defaults() {
            let mut dsp = d.build(48_000.0);
            let mut l = vec![0.5f32; 512];
            let mut r = vec![-0.5f32; 512];
            dsp.process(&mut l, &mut r);
            dsp.update(&d);
            dsp.process(&mut l, &mut r);
            dsp.reset();
            assert!(
                l.iter().chain(r.iter()).all(|s| s.is_finite()),
                "{} produced non-finite samples",
                d.name()
            );
        }
    }
}
