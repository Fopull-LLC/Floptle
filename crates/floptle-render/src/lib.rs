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
//! - `frame`      : per-frame orchestration, culling, draw submission; uploads
//!                  positions **camera-relative** so the GPU never sees large
//!                  coordinates — large-world-safe by default (ADR-0015).

/// Backends Floptle can target through wgpu. Mac uses Metal automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Vulkan,
    Metal,
    Dx12,
    Gl,
}
