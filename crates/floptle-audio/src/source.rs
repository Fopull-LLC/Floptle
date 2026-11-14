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
    /// Every spelling [`Self::parse`] accepts, for an error message that names
    /// what it takes. `spatial_mode_accepts_every_name_it_offers`
    /// pushes all of these through the real parser, so the list cannot drift
    /// from the behaviour.
    pub const ACCEPTS: &'static [&'static str] =
        &["spatial", "3d", "distance", "flat", "2d"];

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
    /// Every spelling [`Self::parse`] accepts.
    pub const ACCEPTS: &'static [&'static str] =
        &["inverse", "linear", "exponential", "exp"];

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
    /// Every spelling [`Self::parse`] accepts.
    pub const ACCEPTS: &'static [&'static str] = &["stop", "destroy", "loop"];

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
    /// Wet taps into other tracks, beside the dry path through `track`: a
    /// sound that is partly in a small room and partly in a hall sends to
    /// both reverbs at once. Taken after the sound's own volume and distance,
    /// so a far sound sends less.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sends: Vec<AuxSend>,
    /// A low-pass on this sound alone, in Hz: a voice behind a wall. 0 = off.
    #[serde(skip_serializing_if = "is_zero")]
    pub lowpass: f32,
    /// The voice group this sound counts against, for a per-group voice
    /// limit ([`crate::voice::AudioCore::set_group_limit`]). Empty = none.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub group: String,
    /// Which sound gives way when its group is full: the lowest goes first.
    #[serde(skip_serializing_if = "is_zero_i32")]
    pub priority: i32,
    /// Air absorption: how fast distance dulls this sound. Past
    /// `min_distance` its cutoff falls as `20 kHz / (1 + k × metres)`, so
    /// 0.02 is a sound across a field, 0.2 one through thick fog. 0 = off.
    /// Only a sound placed in the world has a distance.
    #[serde(skip_serializing_if = "is_zero")]
    pub air_absorption: f32,
    /// A low-pass that closes with distance: `near` Hz at `min_distance`,
    /// `far` Hz at `max_distance`, gliding evenly in pitch between them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lowpass_falloff: Option<LowpassFalloff>,
    /// Heard inside the listener's own space (a helmet, a cockpit), not out
    /// in the world: the world low-pass
    /// ([`crate::voice::AudioCore::set_world_lowpass`]) passes it by.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub listener_space: bool,
}

/// A distance low-pass curve: the cutoff at a sound's `min_distance` and at
/// its `max_distance` ([`PlayParams::lowpass_falloff`]).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LowpassFalloff {
    pub near: f32,
    pub far: f32,
}

fn is_zero(x: &f32) -> bool {
    *x == 0.0
}

fn is_zero_i32(x: &i32) -> bool {
    *x == 0
}

/// One wet tap: how much of a sound also goes into another track.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuxSend {
    pub track: String,
    /// Linear, 0..1 (more is allowed, up to 4).
    pub level: f32,
}

impl PlayParams {
    /// Set the send into `track` to `level`; 0 or less removes it.
    pub fn set_send(&mut self, track: &str, level: f32) {
        self.sends.retain(|s| s.track != track);
        if level > 0.0 {
            self.sends.push(AuxSend { track: track.to_string(), level: level.min(4.0) });
        }
    }
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
            sends: Vec::new(),
            lowpass: 0.0,
            group: String::new(),
            priority: 0,
            air_absorption: 0.0,
            lowpass_falloff: None,
            listener_space: false,
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

#[cfg(test)]
mod accepts_tests {
    use super::*;

    /// Every name an error message offers has to parse.
    ///
    /// The failure this prevents is the worst kind of documentation bug: an
    /// error that tells you to write something the code then refuses, so the
    /// reader concludes the engine is broken — and they are right, just not
    /// about what.
    #[test]
    fn every_offered_audio_enum_name_parses() {
        for s in SpatialMode::ACCEPTS {
            assert!(SpatialMode::parse(s).is_some(), "offered but refused: mode = {s:?}");
        }
        for s in Falloff::ACCEPTS {
            assert!(Falloff::parse(s).is_some(), "offered but refused: falloff = {s:?}");
        }
        for s in EndBehavior::ACCEPTS {
            assert!(EndBehavior::parse(s).is_some(), "offered but refused: endBehavior = {s:?}");
        }
        // …and the case the whole task is about: a near-miss is refused, not
        // quietly defaulted.
        assert!(SpatialMode::parse("spacial").is_none());
        assert!(Falloff::parse("exponentail").is_none());
    }
}
