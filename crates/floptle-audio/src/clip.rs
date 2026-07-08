//! A decoded audio clip: interleaved f32 PCM at its native sample rate.
//! Voices resample on the fly (which is also how pitch works), so clips are
//! stored exactly as decoded — no offline conversion pass.

use std::sync::Arc;

/// Immutable decoded audio. Shared between the control side and playing
/// voices via `Arc`, so playing the same clip 50 times costs 50 playheads,
/// not 50 copies of the samples.
#[derive(Debug, Clone)]
pub struct Clip {
    /// Native sample rate of the decoded data.
    pub sample_rate: u32,
    /// 1 (mono) or 2 (stereo); >2-channel sources are downmixed at decode.
    pub channels: u16,
    /// Interleaved samples, `frames() * channels` long, in -1..1.
    pub samples: Vec<f32>,
}

pub type ClipRef = Arc<Clip>;

impl Clip {
    /// Number of sample frames (samples per channel).
    pub fn frames(&self) -> usize {
        if self.channels == 0 { 0 } else { self.samples.len() / self.channels as usize }
    }

    /// Clip length in seconds.
    pub fn duration_secs(&self) -> f32 {
        if self.sample_rate == 0 { 0.0 } else { self.frames() as f32 / self.sample_rate as f32 }
    }

    /// Stereo sample at a fractional frame position, linearly interpolated.
    /// Mono clips return the same value on both sides. Out-of-range reads
    /// return silence (voices handle looping by wrapping `pos` themselves).
    #[inline]
    pub fn sample_at(&self, pos: f64) -> (f32, f32) {
        let frames = self.frames();
        if frames == 0 || pos < 0.0 {
            return (0.0, 0.0);
        }
        let i0 = pos as usize;
        if i0 + 1 >= frames {
            // Last frame (or past it): no next frame to lerp toward.
            if i0 >= frames {
                return (0.0, 0.0);
            }
            let c = self.channels as usize;
            let l = self.samples[i0 * c];
            let r = if c > 1 { self.samples[i0 * c + 1] } else { l };
            return (l, r);
        }
        let frac = (pos - i0 as f64) as f32;
        let c = self.channels as usize;
        let (l0, r0, l1, r1) = if c > 1 {
            (
                self.samples[i0 * c],
                self.samples[i0 * c + 1],
                self.samples[(i0 + 1) * c],
                self.samples[(i0 + 1) * c + 1],
            )
        } else {
            let a = self.samples[i0];
            let b = self.samples[i0 + 1];
            (a, a, b, b)
        };
        (l0 + (l1 - l0) * frac, r0 + (r1 - r0) * frac)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_lerp_and_bounds() {
        let clip = Clip { sample_rate: 4, channels: 1, samples: vec![0.0, 1.0, 0.0, -1.0] };
        assert_eq!(clip.frames(), 4);
        assert_eq!(clip.duration_secs(), 1.0);
        let (l, r) = clip.sample_at(0.5);
        assert!((l - 0.5).abs() < 1e-6 && l == r);
        assert_eq!(clip.sample_at(99.0), (0.0, 0.0));
    }

    #[test]
    fn stereo_channels_stay_separate() {
        let clip = Clip { sample_rate: 2, channels: 2, samples: vec![1.0, -1.0, 1.0, -1.0] };
        let (l, r) = clip.sample_at(0.0);
        assert_eq!((l, r), (1.0, -1.0));
    }
}
