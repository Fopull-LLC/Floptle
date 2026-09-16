//! # floptle-core
//!
//! The foundation every other crate builds on: the ECS, the node and
//! component types, transforms, time and the world's reference frames.
//! Deliberately small and data-oriented. See `docs/subsystems/scene-and-nodes.md`.
//!
//! - `ecs`      — archetype ECS, the runtime under everything.
//! - `matter`   — what a node is: primitives, models, cameras, lights, tilemaps,
//!   sprites, probes, and the physics body.
//! - `material` — materials and tints.
//! - `transform`, `origin`, `frames` — `f64` world transforms, the floating
//!   origin, and hierarchical reference frames (galaxy → system → body → local),
//!   large-world-safe by default (ADR-0015).
//! - `time`     — the frame clock, fixed timestep and timers (ADR-0017).
//! - `layers`, `tile`, `camera2d`, `scatter`, `spatial`, `noise`, `net`,
//!   `profile`, `access`, `event`, `script` — the smaller building blocks named
//!   after what they hold.

#![forbid(unsafe_op_in_unsafe_fn)]

pub mod access;
pub mod camera2d;
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
pub use material::{Material, ObjectMaterials, Retro, Shading, Tiling, Tint};
pub use matter::{
    TerrainCollision,
    active_camera, is_disabled, is_drawn, is_persistent, is_spot, world_transform, MIN_SPOT_ANGLE, OMNI_ANGLE, AnimController, AoMode, BodyKind, BodyMode, BoneAttach, Cast2D,
    CastShadow,
    CelestialBody, Collidable, Disabled, GravityMode, Layer, Light, LightShape, Lighting2D, Lit2D, Lit2DFacts, Made, Matter,
    MeshCollider, Name, NavMeshExclude,
    Parent,
    Parallax, ParticleSystem, Persistent, RepeatIndex, RigidBody, SceneTag, ScreenShader, Shadow2D, Shape, SortMode, Sorting, Spin, Sprite, Sprites,
    Tags,
    TerrainGen, TexturePaint, Trigger, VertexPaint, WaterKind,
    Visible, DEFAULT_SORTING_LAYER, EMPTY_TILE, SORT_LAYER_STEP, SORT_ORDER_STEP,
    SORT_Y_BANDS, infers_2d, rank_offset, resolve_2d, resolve_shadow_2d, sorting_offset,
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
