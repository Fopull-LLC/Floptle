//! # floptle-core
//!
//! The foundation every other crate builds on. Deliberately tiny and
//! data-oriented. See `docs/subsystems/scene-and-nodes.md`.
//!
//! Planned modules (added as each lands — kept stubbed during planning):
//! - `math`    : thin re-exports / helpers over `glam`.
//! - `ecs`     : archetype ECS — the data-oriented runtime under everything.
//! - `scene`   : the Node + Component *authoring facade* over the ECS.
//! - `transform`: high-precision (`f64`/`DVec3`) world transform + a derived
//!   camera-relative `f32` render transform — large-world-safe by
//!   default (ADR-0015).
//! - `origin`  : floating origin — keeps the active sim near `(0,0,0)` and rebases
//!   the world around the player so distance never jitters.
//! - `frames`  : hierarchical reference frames (galaxy→system→body→local).
//! - `event`   : engine + input + dialogue event bus.
//! - `time`    : frame clock, fixed timestep, timers; per-entity `LocalTime` +
//!   the time-rate field `r(p)` for slow/freeze/dilation (ADR-0017).
//! - `pool`    : automatic object pooling (see ADR-0008, the "take/return" API).
//! - `serde_ron`: scene/prefab (de)serialization helpers (RON).

#![forbid(unsafe_op_in_unsafe_fn)]

// Phase 1 modules (the foundation the frame loop hangs on). `scene`, `pool`,
// and `serde_ron` arrive in their roadmap phases; these are live.
pub mod ecs;
pub mod event;
pub mod frames;
pub mod layers;
pub mod material;
pub mod math;
pub mod matter;
pub mod net;
pub mod noise;
pub mod origin;
pub mod script;
pub mod time;
pub mod transform;

pub use ecs::{Entity, World};
pub use layers::Layers;
pub use material::{Material, Tiling};
pub use matter::{
    world_transform, AnimController, AoMode, BodyKind, BodyMode, BoneAttach, CastShadow,
    Collidable, GravityMode, Layer, Light, Matter, MeshCollider, Name, Parent, ParticleSystem,
    RigidBody, Shape, Spin, Tags, TexturePaint, Trigger, VertexPaint, Visible,
};
pub use net::{NetId, Replicated, ReplicationMode};
pub use script::{ScriptInst, Scripts};
pub use origin::FloatingOrigin;
pub use time::{FixedTimestep, Time};
pub use transform::Transform;

/// Engine-wide version string, surfaced in the editor title bar and crash logs.
pub const ENGINE_NAME: &str = "Floptle";
pub const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");
