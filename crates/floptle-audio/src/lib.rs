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
//! - [`stream`]: a live sample ring — what makes a remote player's microphone
//!   an ordinary sound in the world rather than a special case beside it.
//! - [`voice`]: playing voices + [`voice::AudioCore`], the pure render core.
//! - `engine`/`decode`/`chat`/`capture` (feature `backend`): the cpal output
//!   stream, symphonia file decoding, and voice chat (Opus + a jitter buffer +
//!   microphone capture). Off by default for data-model crates so they don't
//!   link the OS audio stack.
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
pub mod stream;
pub mod voice;

#[cfg(feature = "backend")]
pub mod capture;
#[cfg(feature = "backend")]
pub mod chat;
#[cfg(feature = "backend")]
pub mod decode;
#[cfg(feature = "backend")]
pub mod engine;
/// The browser's output scheduling. Its cursor rule is a plain function with
/// plain tests, so it is compiled (and tested) everywhere; only the Web Audio
/// glue is gated to wasm — which is also why the rule reads as dead code on
/// every other target.
#[cfg(feature = "backend")]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod web_out;

pub use clip::{Clip, ClipRef};
pub use effects::{EffectDesc, EqBand, EqBandKind};
pub use mixer::{EffectSlot, MixerDesc, TrackDesc, MASTER};
pub use source::{AudioSource, EndBehavior, Falloff, PlayParams, SpatialMode};
pub use spatial::Listener;
pub use stream::{StreamRef, StreamRing, STREAM_RATE};
pub use voice::{VoiceId, VoiceStatus};

#[cfg(feature = "backend")]
pub use capture::Capture;
#[cfg(feature = "backend")]
pub use chat::{VoiceDecoder, VoiceEncoder, VoiceJitter, FRAME_MS, FRAME_SAMPLES};
#[cfg(feature = "backend")]
pub use decode::{is_audio_path, load_clip, AUDIO_EXTENSIONS};
#[cfg(feature = "backend")]
pub use engine::AudioEngine;
