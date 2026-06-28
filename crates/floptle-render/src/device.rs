//! wgpu bootstrap: instance → adapter → device/queue → surface. The single GPU
//! handle the whole renderer threads through (`docs/subsystems/renderer.md`).
//!
//! wgpu is *only* the portability layer (ADR-0002) — everything above this (the
//! render graph, passes, the SDF/raymarch look) is ours. The lifecycle here
//! (resize, acquire, present) is real; `Gpu::new` is the one call that needs a
//! live window, and a battle-tested implementation already exists in
//! `crates/floptle-proof/src/main.rs` (wgpu 29) — Phase 1 lifts it here.

use std::sync::Arc;
use winit::window::Window;

/// Owns the GPU connection and the window surface.
pub struct Gpu {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub surface: wgpu::Surface<'static>,
    pub config: wgpu::SurfaceConfiguration,
}

/// A surface image acquired for one frame. Render into `view`, then `present()`.
pub struct Frame {
    pub surface: wgpu::SurfaceTexture,
    pub view: wgpu::TextureView,
}

impl Frame {
    /// Hand the finished image to the compositor.
    pub fn present(self) {
        self.surface.present();
    }
}

impl Gpu {
    /// Create the GPU connection for `window` and configure its surface.
    ///
    /// Fill-in (Phase 1) — mirror the proof's working sequence:
    /// 1. `wgpu::Instance::new` (PRIMARY backends).
    /// 2. `instance.create_surface(window.clone())` (the `Arc<Window>` keeps it `'static`).
    /// 3. `pollster::block_on(instance.request_adapter(..compatible_surface..))`.
    /// 4. `pollster::block_on(adapter.request_device(..))` for device + queue.
    /// 5. Pick a surface format from `surface.get_capabilities(&adapter)`, build a
    ///    `SurfaceConfiguration` at the window's inner size, `surface.configure(..)`.
    pub fn new(window: Arc<Window>) -> Self {
        let _ = window;
        todo!("Phase 1: lift the wgpu-29 bootstrap from crates/floptle-proof/src/main.rs")
    }

    /// The surface's swapchain format — every pass that targets the screen needs it.
    pub fn surface_format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    /// Reconfigure the surface after the window resizes. Clamps to a minimum of 1
    /// so a minimized window (0×0) doesn't produce an invalid configuration.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        self.surface.configure(&self.device, &self.config);
    }

    /// Acquire the next swapchain image to render into. Returns `None` on a
    /// transient surface state (Outdated/Lost — reconfigured here) or failure, in
    /// which case the caller simply skips the frame.
    pub fn acquire(&mut self) -> Option<Frame> {
        use wgpu::CurrentSurfaceTexture as C;
        let surface = match self.surface.get_current_texture() {
            C::Success(t) | C::Suboptimal(t) => t,
            C::Outdated | C::Lost => {
                self.surface.configure(&self.device, &self.config);
                return None;
            }
            _ => return None,
        };
        let view = surface.texture.create_view(&wgpu::TextureViewDescriptor::default());
        Some(Frame { surface, view })
    }
}
