//! # floptle-vfx
//!
//! The particle system you actually *want* to use. A particle **effect** (e.g.
//! "360Slash") owns a timeline spanning its lifetime. On the timeline you place
//! particle **groups** (e.g. "Crescents", "Smoke"), each with its own behavior
//! and `Emit` events — either auto-generated from an emission rate or hand-
//! placed. Every property is a constant OR a curve over the particle's life.
//! See `docs/subsystems/particles-vfx.md` — this is the spec'd, opinionated flow.
//!
//! Planned modules:
//! - `effect`   : ParticleEffect (name, lifetime, loop/oneshot, end behavior).
//! - `timeline` : events, tracks, the `Emit` event, playback cursor.
//! - `group`    : ParticleGroup behavior + property set.
//! - `curve`    : value-OR-curve property type; the graph editor backing data.
//! - `sim`      : the data-oriented particle simulation (GPU-friendly).

/// How an effect behaves when its lifetime elapses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndBehavior {
    /// One-shot effect that despawns itself once the last particle dies.
    Destroy,
    /// One-shot effect that persists (frozen) after its lifetime.
    Persist,
    /// Looping effect — restarts at t=0; `EndBehavior` is not offered in UI.
    Loop,
}
