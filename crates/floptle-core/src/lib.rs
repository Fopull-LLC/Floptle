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
pub mod access;
pub mod ecs;
pub mod event;
pub mod frames;
pub mod scatter;
pub mod layers;
pub mod material;
pub mod math;
pub mod matter;
pub mod net;
pub mod noise;
pub mod origin;
pub mod profile;
pub mod script;
pub mod spatial;
pub mod tile;
pub mod time;
pub mod transform;

pub use ecs::{Entity, World};
pub use layers::Layers;
pub use material::{Material, ObjectMaterials, Tiling};
pub use matter::{
    is_disabled, is_persistent, world_transform, AnimController, AoMode, BodyKind, BodyMode, BoneAttach, Cast2D,
    CastShadow,
    CelestialBody, Collidable, Disabled, GravityMode, Layer, Light, Lighting2D, Lit2D, Lit2DFacts, Made, Matter,
    MeshCollider, Name,
    Parent,
    ParticleSystem, Persistent, RepeatIndex, RigidBody, SceneTag, Shadow2D, Shape, Sorting, Spin, Sprite, Sprites,
    Tags,
    TerrainGen, TexturePaint, Trigger, VertexPaint, WaterKind,
    Visible, DEFAULT_SORTING_LAYER, EMPTY_TILE, SORT_LAYER_STEP, SORT_ORDER_STEP,
    infers_2d, resolve_2d, resolve_shadow_2d, sorting_offset,
};
pub use net::{NetId, Replicated, ReplicationMode};
pub use tile::{
    tile_cell_of, tile_corner, tile_corner_drawn, tile_in_page, tile_index, tile_is_empty,
    tile_pack, tile_page, tile_point_drawn, tile_reoriented, tile_xform, TileXform,
    TILE_CELL_MASK, TILE_FLIP_X, TILE_MAX_PAGES, TILE_PAGE_BITS, TILE_PAGE_STRIDE,
    TILE_ROT_SHIFT, TILE_XFORM_MASK,
};
pub use script::{ScriptInst, Scripts};
pub use origin::FloatingOrigin;
pub use time::{FixedTimestep, Time};
pub use transform::Transform;

/// Engine-wide version string, surfaced in the editor title bar and crash logs.
pub const ENGINE_NAME: &str = "Floptle";
pub const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");
