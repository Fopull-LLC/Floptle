//! # floptle-render
//!
//! Owns *everything* that defines how Floptle looks. wgpu is only the
//! portability layer (ADR-0002); the render graph, passes, and the
//! "reality-bending" look are all ours. See `docs/subsystems/renderer.md`.
//!
//! Planned modules:
//! - `device`     : wgpu instance/adapter/device/surface bootstrap.
//! - `graph`      : the render graph (passes, resources, dependencies).
//! - `mesh`       : GPU mesh upload + dynamic/morphing vertex buffers.
//! - `material`   : material model binding shaders + textures + params.
//! - `raymarch`   : SDF / fractal raymarching pass — "go inside the fractal".
//! - `post`       : screen-space passes that bend conventional light rules.
//! - `light`      : programmable light transport — tiered, off-by-default; bent
//!                  rays where `bend` can BE `g(p)`, so light falls under your
//!                  custom gravity (one rule, two phenomena) (ADR-0016).
//! - `frame`      : per-frame orchestration, culling, draw submission; uploads
//!                  positions **camera-relative** so the GPU never sees large
//!                  coordinates — large-world-safe by default (ADR-0015).

// Phase 1–2 modules. `material`, `raymarch`, `post`, `light` arrive in Phases 2/4.
// `mesh` is the CPU/GPU geometry seam (Phase 2); `raster` is the forward pass.
pub mod camera;
pub mod device;
pub mod frame;
pub mod graph;
pub mod grid;
pub mod mesh;
pub mod outline;
pub mod raster;
pub mod raymarch;
pub mod retro;

pub use camera::{FlyCamera, Input};
pub use device::Gpu;
pub use frame::{Projection, RenderCamera};
pub use grid::Grid;
pub use mesh::{cube, uv_sphere, GpuMesh, MeshData, MeshId, TextureData, Vertex};
pub use outline::Outline;
pub use raster::{instance_of, instance_of_mat, Globals, InstanceRaw, MaterialParams, Raster, TexId};
pub use raymarch::{Raymarch, RaymarchGlobals};
pub use retro::Retro;

/// Backends Floptle can target through wgpu. Mac uses Metal automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Vulkan,
    Metal,
    Dx12,
    Gl,
}
