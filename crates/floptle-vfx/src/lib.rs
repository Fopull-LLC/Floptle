//! # floptle-vfx
//!
//! The particle system you actually *want* to use. A particle **effect** (e.g.
//! "360Slash") is a container of **tracks** arranged on a video-editor timeline:
//! each track is one visual layer with its own look, carrying draggable **clips**
//! (ranged emission spans), **bursts** (instant emits), and **automation lanes**
//! (curves over effect time). Every per-particle property is a constant OR a curve
//! over the particle's own life. Design: `docs/particle-system-proposal.md`.
//!
//! Layering: authoring types ([`ParticleEffect`]) are what the editor edits and RON
//! round-trips; [`CompiledEffect`] is the LUT-baked form the deterministic SoA sim
//! ([`EffectInstance`]) runs. The sim is CPU today and GPU-ready by construction
//! (proposal §4.4): LUT curves, births-on-CPU, vec4-aligned SoA, stateless hash RNG.

pub mod curve;
pub mod draw;
pub mod effect;
pub mod sim;

pub use curve::{Curve, Extrapolate, Interp, Key, Value, ValueOrCurve};
pub use draw::{BillboardDraw, collect_billboards};
pub use effect::{
    Blend, Burst, Clip, CompiledEffect, CompiledTrack, EmitShape, EndBehavior, Lane, LaneTarget,
    Look, ParticleEffect, Playback, RenderMode, Space, Track,
};
pub use sim::{EffectInstance, ParticleSample, SCRUB_STEP};
