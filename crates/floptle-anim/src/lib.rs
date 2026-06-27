//! # floptle-anim
//!
//! Exactly enough animation for surreal adventures and flashy combat — clean
//! states, fast attacks/movement, snappy transitions. No bloat. Clips come from
//! Blender via glTF. See `docs/subsystems/animation.md`.
//!
//! Planned modules:
//! - `clip`     : sampled skeletal animation clips (from glTF).
//! - `skeleton` : joint hierarchy + pose/skinning matrices.
//! - `state`    : the animation state machine (states, transitions, conditions).
//! - `blend`    : crossfade + additive blends (movement <-> attack).
//! - `events`   : animation notify events (e.g. "spawn 360Slash on frame 7").
