//! # floptle-core
//!
//! The foundation every other crate builds on. Deliberately tiny and
//! data-oriented. See `docs/subsystems/scene-and-nodes.md`.
//!
//! Planned modules (added as each lands — kept stubbed during planning):
//! - `math`    : thin re-exports / helpers over `glam`.
//! - `ecs`     : archetype ECS — the data-oriented runtime under everything.
//! - `scene`   : the Node + Component *authoring facade* over the ECS.
//! - `event`   : engine + input + dialogue event bus.
//! - `time`    : frame clock, fixed timestep, timers.
//! - `pool`    : automatic object pooling (see ADR-0008, the "take/return" API).
//! - `serde_ron`: scene/prefab (de)serialization helpers (RON).

#![forbid(unsafe_op_in_unsafe_fn)]

/// Engine-wide version string, surfaced in the editor title bar and crash logs.
pub const ENGINE_NAME: &str = "Floptle";
pub const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");
