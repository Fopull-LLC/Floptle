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
    /// Create the GPU connection for `window` and configure its surface. Picks a
    /// high-performance adapter, an sRGB surface format when available, and Mailbox
    /// present mode (low-latency) falling back to Fifo (vsync). Lifted from the
    /// proof's proven wgpu-29 bootstrap.
    pub fn new(window: Arc<Window>) -> Self {
        let size = window.inner_size();

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: Default::default(),
            backend_options: Default::default(),
            display: None,
        });
        let surface = instance.create_surface(window).expect("create surface");

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .expect("no compatible GPU adapter");

        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("floptle-device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .expect("no GPU device");

        let caps = surface.get_capabilities(&adapter);
        let format =
            caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(caps.formats[0]);
        let present_mode = if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        };
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        Self { instance, adapter, device, queue, surface, config }
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

    /// Clear a frame to a solid linear-RGBA color — the minimal Phase-1 render so
    /// the window proves the whole window→device→loop→present path. The render
    /// graph + real passes supersede this in Phase 4; it keeps `wgpu` out of the
    /// runtime in the meantime.
    pub fn clear(&self, frame: &Frame, color: [f64; 4]) {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("clear") });
        {
            let _rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &frame.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: color[0],
                            g: color[1],
                            b: color[2],
                            a: color[3],
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        self.queue.submit([encoder.finish()]);
    }
}
