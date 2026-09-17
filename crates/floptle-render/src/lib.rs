//! # floptle-render
//!
//! Everything that defines how Floptle looks. wgpu is the portability layer;
//! the passes and the look are the engine's own. See `docs/subsystems/renderer.md`.
//!
//! - `device` — the wgpu instance, adapter, device and surface.
//! - `frame`, `camera`, `cull` — per-frame orchestration, projections, frustum culling.
//! - `raster`, `mesh`, `lines`, `tris`, `grid`, `outline` — the forward mesh pass and the editor's overlays.
//! - `raymarch` — the signed-distance-field world: terrain, blobs, the sky.
//! - `light2d`, `palette`, `retro` — the flat-scene light pass, palette quantise and low-res upscale.
//! - `env`, `reflect`, `ssr`, `gi` — the sky capture, reflection probes, the scene history, baked probes.
//! - `particles`, `post`, `ui` — billboards, the post chain, the screen-space UI.
//! - `gpu_timer`, `probe` — per-pass timing and headless readback for the probes.
//!
//! Positions reach the GPU camera-relative, so a world can be planet-sized
//! without the precision problems of large coordinates.

// Phase 1–2 modules. `material`, `raymarch`, `post`, `light` arrive in Phases 2/4.
// `mesh` is the CPU/GPU geometry seam (Phase 2); `raster` is the forward pass.
pub mod camera;
pub mod cull;
pub mod device;
pub mod env;
pub mod frame;
pub mod gi;
pub mod gpu_timer;
pub mod graph;
pub mod grid;
pub mod light2d;
pub mod lines;
pub mod mesh;
pub mod outline;
pub mod palette;
pub mod particles;
pub mod post;
pub mod probe;
pub mod raster;
pub mod raymarch;
pub mod retro;
pub mod reflect;
pub mod ssr;
pub mod tris;
pub mod ui;

pub use camera::{FlyCamera, Input, ViewLock};
pub use cull::Frustum;
pub use device::{Gpu, Vsync, take_gpu_errors};
pub use env::{EnvMap, ENV_H, ENV_W};
pub use frame::{ORTHO_DEPTH, Projection, RenderCamera};
pub use gi::GiVolume;
pub use grid::Grid;
pub use light2d::{Light2d, Light2dInstance, Light2dUniform};
pub use lines::{LineVertex, Lines};
pub use tris::{TriVertex, Tris};
pub use mesh::{
    capsule, chunk_mesh_data, cone, cube, cylinder, plane, pyramid, uv_sphere, GpuMesh, MeshData,
    MeshId, TextureData, Vertex,
};
pub use outline::Outline;
pub use gpu_timer::{GpuTimer, Span};
pub use reflect::{MAX_PROBES, PROBE_FACE, PROBE_H, PROBE_W, ProbeDetail, ReflectionProbes};
pub use ssr::SceneHistory;
pub use palette::{Palette, PaletteQuantize};
pub use particles::{ParticleBatch, ParticleBlend, ParticleGlobals, ParticleInstance, Particles};
pub use post::{PostSettings, PostShaderId, PostShaders, PostStack, SsaoFrame, Tonemap};
pub use ui::{Ui, UiBatch, UiBindingId, UiInstance, UiPlane, UiShaderId, UiTex};
pub use raster::{
    ext_index_of, instance_of, instance_of_mat, pass_prelude, raster_custom_source, set_ext_index,
    FlslBindingId, FlslBlend, FlslDraw, FlslShaderId, Globals, InstanceRaw, MaterialParams, Raster,
    SkinDraw, SurfaceExtras, TexFilter, TexId, TexSampling, TexWrap,
};
pub use raymarch::{
    Raymarch, RaymarchGlobals, MAX_FIELD_SHAPES, MAX_SHADOW_PROXIES, MAX_VOLUMES, TERRAIN_SLOTS,
};
pub use retro::Retro;

/// Backends Floptle can target through wgpu. Mac uses Metal automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Vulkan,
    Metal,
    Dx12,
    Gl,
}
