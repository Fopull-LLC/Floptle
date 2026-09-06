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
pub use decode::load_clip;

/// The audio containers this engine can open.
///
/// **Not behind `backend`, on purpose.** It is a list of four strings, and the
/// question it answers — "is this file a sound?" — is one the Assets browser
/// asks whether or not this build has an audio device. Gating it with the
/// decoder made a dedicated server's build fail on a const array, which is a
/// silly reason not to compile.
pub const AUDIO_EXTENSIONS: &[&str] = &["wav", "ogg", "mp3", "flac"];

/// Does this path name a sound file? See [`AUDIO_EXTENSIONS`].
pub fn is_audio_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

#[cfg(feature = "backend")]
pub use engine::AudioEngine;

#[cfg(test)]
mod path_tests {
    use super::*;
    use std::path::Path;

    /// Moved here with the function itself, rather than left behind in
    /// `decode.rs` where it would have stopped compiling: a test travels with
    /// the thing it tests. It also has to keep running in a build with **no
    /// decoder**, which is the whole reason the function moved.
    #[test]
    fn a_sound_is_recognised_by_its_extension_whatever_the_case() {
        assert!(is_audio_path(Path::new("tone.wav")));
        assert!(is_audio_path(Path::new("music.OGG")), "case does not decide this");
        assert!(is_audio_path(Path::new("a/b/c.flac")));
        assert!(!is_audio_path(Path::new("foo.png")));
        assert!(!is_audio_path(Path::new("no-extension")));
    }
}
