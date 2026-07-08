//! The `AudioSource` component and the shared playback parameter types.
//!
//! `AudioSource` is a plain ECS component (inserted by the scene loader /
//! inspector, like `ParticleSystem`); the audio engine reads it each frame and
//! keeps a voice in sync with it. `PlayParams` is the same knob set for
//! fire-and-forget one-shots (`audio.play` in Lua).

use serde::{Deserialize, Serialize};

/// How a sound sits in the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SpatialMode {
    /// Full 3D: distance attenuation + directional stereo panning.
    #[default]
    Spatial,
    /// Distance attenuation only — same loudness curve, no panning.
    Distance,
    /// Plain 2D playback (UI, music): ignores positions entirely.
    Flat,
}

/// Distance attenuation curve between `min_distance` and `max_distance`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Falloff {
    /// Real-world-ish 1/d rolloff (default): fast near, gentle far.
    #[default]
    Inverse,
    /// Straight line from full volume at min to silent at max.
    Linear,
    /// Steeper-than-inverse rolloff; drops hard right past min.
    Exponential,
}

/// What happens when a non-looping sound finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum EndBehavior {
    /// The voice ends; the source/node sticks around and can be replayed.
    #[default]
    Stop,
    /// The node the sound was playing on is despawned (one-shot SFX).
    Destroy,
    /// Seamlessly restart from the beginning.
    Loop,
}

impl SpatialMode {
    /// Parse the user-facing (Lua/inspector) spelling; case-insensitive.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "spatial" | "3d" => Some(Self::Spatial),
            "distance" => Some(Self::Distance),
            "flat" | "2d" => Some(Self::Flat),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Spatial => "Spatial",
            Self::Distance => "Distance",
            Self::Flat => "Flat",
        }
    }
}

impl Falloff {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "inverse" => Some(Self::Inverse),
            "linear" => Some(Self::Linear),
            "exponential" | "exp" => Some(Self::Exponential),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Inverse => "Inverse",
            Self::Linear => "Linear",
            Self::Exponential => "Exponential",
        }
    }
}

impl EndBehavior {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "stop" => Some(Self::Stop),
            "destroy" => Some(Self::Destroy),
            "loop" => Some(Self::Loop),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Stop => "Stop",
            Self::Destroy => "Destroy",
            Self::Loop => "Loop",
        }
    }
}

/// Everything tweakable about a playing sound. Used verbatim by one-shots and
/// mirrored by the `AudioSource` component.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PlayParams {
    /// Linear volume, 0..2 (1 = as authored).
    pub volume: f32,
    /// Playback rate multiplier; also shifts pitch (0.5 = octave down).
    pub pitch: f32,
    /// Manual stereo pan -1..1 (only meaningful for non-Spatial modes).
    pub pan: f32,
    pub mode: SpatialMode,
    pub falloff: Falloff,
    /// Distance at which attenuation starts (full volume inside).
    pub min_distance: f32,
    /// Distance at which the sound is fully silent.
    pub max_distance: f32,
    /// Mixer track this sound routes through; empty = Master.
    pub track: String,
    pub end: EndBehavior,
}

impl Default for PlayParams {
    fn default() -> Self {
        Self {
            volume: 1.0,
            pitch: 1.0,
            pan: 0.0,
            mode: SpatialMode::Spatial,
            falloff: Falloff::Inverse,
            min_distance: 2.0,
            max_distance: 50.0,
            track: String::new(),
            end: EndBehavior::Stop,
        }
    }
}

/// A sound emitter on a scene node. `clip` is a project-relative asset path
/// (e.g. `"audio/steps.ogg"`), same convention as mesh/texture references.
/// Serialized directly into scene RON (it doubles as its own Doc type).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioSource {
    pub clip: String,
    pub params: PlayParams,
    /// Start playing as soon as Play mode starts.
    pub play_on_start: bool,
}

impl Default for AudioSource {
    fn default() -> Self {
        Self { clip: String::new(), params: PlayParams::default(), play_on_start: true }
    }
}
