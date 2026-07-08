//! floptle-audio — the engine's sound system.
//!
//! Layers, bottom up:
//! - [`clip`]: decoded PCM, shared by `Arc`.
//! - [`effects`]: serializable effect descriptors + real-time DSP (EQ, delay,
//!   reverb, chorus/flanger/phaser, pitch shift, dynamics, distortion…).
//! - [`mixer`]: named tracks with gain/pan/mute/solo + effect chains, routing
//!   into each other and ultimately into Master.
//! - [`spatial`]: distance falloff curves + directional panning (large-world
//!   f64 positions).
//! - [`source`]: the `AudioSource` ECS component and `PlayParams` (the shared
//!   knob set for components and one-shots).
//! - [`voice`]: playing voices + [`voice::AudioCore`], the pure render core.
//! - `engine`/`decode` (feature `backend`): the cpal output stream and
//!   symphonia file decoding. Off by default for data-model crates so they
//!   don't link the OS audio stack.
//!
//! Real-time discipline: the audio callback never locks or allocates on the
//! steady path. Control threads talk to it through a command channel; status
//! flows back through `try_lock`ed snapshots that skip a frame under
//! contention rather than stall the mix.

pub mod clip;
pub mod effects;
pub mod mixer;
pub mod source;
pub mod spatial;
pub mod voice;

#[cfg(feature = "backend")]
pub mod decode;
#[cfg(feature = "backend")]
pub mod engine;

pub use clip::{Clip, ClipRef};
pub use effects::{EffectDesc, EqBand, EqBandKind};
pub use mixer::{EffectSlot, MixerDesc, TrackDesc, MASTER};
pub use source::{AudioSource, EndBehavior, Falloff, PlayParams, SpatialMode};
pub use spatial::Listener;
pub use voice::{VoiceId, VoiceStatus};

#[cfg(feature = "backend")]
pub use decode::{is_audio_path, load_clip, AUDIO_EXTENSIONS};
#[cfg(feature = "backend")]
pub use engine::AudioEngine;
