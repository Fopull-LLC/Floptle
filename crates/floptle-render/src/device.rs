//! wgpu bootstrap: instance → adapter → device/queue → surface. The single GPU
//! handle the whole renderer threads through (`docs/subsystems/renderer.md`).
//!
//! wgpu is *only* the portability layer (ADR-0002) — everything above this (the
//! render graph, passes, the SDF/raymarch look) is ours. The lifecycle here
//! (resize, acquire, present) is real; `Gpu::new` is the one call that needs a
//! live window, and a battle-tested implementation already exists in
//! `crates/floptle-proof/src/main.rs` (wgpu 29) — Phase 1 lifts it here.

use std::sync::{Arc, Mutex};

use winit::window::Window;

/// GPU errors waiting for the host to pick up, plus every message already
/// reported this session.
///
/// The `seen` half matters as much as the queue: a bad pipeline is rejected on
/// EVERY frame the pass runs, so reporting each occurrence would write sixty
/// identical Console lines a second and bury whatever else is wrong. Each
/// distinct message is said once, which is the same bargain the shader
/// compiler's error reporting already makes.
static GPU_ERRORS: Mutex<(Vec<String>, Vec<String>)> = Mutex::new((Vec::new(), Vec::new()));

/// What went wrong, flattened onto one line.
///
/// `Display` already carries the whole "Caused by:" chain — which pass, which
/// pipeline, which attachment — across several indented lines. That is right
/// for a terminal and wrong for a Console row, so the indentation collapses to
/// single spaces and the whole thing becomes one entry. (Walking `source()` to
/// rebuild the chain by hand is redundant: it only reprints what `Display`
/// already said.)
fn describe(e: &wgpu::Error) -> String {
    let text = match e {
        wgpu::Error::OutOfMemory { .. } => "out of GPU memory".to_string(),
        other => other.to_string(),
    };
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Which graphics backends to consider, honouring `WGPU_BACKEND`.
///
/// **This exists so a backend-specific bug can be reproduced on a machine that
/// would never pick that backend.** Everything here runs on Vulkan in practice;
/// GitHub's runners have no Vulkan driver and fall back to OpenGL, where naga's
/// GLSL output has restrictions the other backends do not — and a failure only
/// CI can see is a failure nobody can fix. `WGPU_BACKEND=gl cargo run …`
/// reproduces it in one command.
///
/// Unset means all of them, which is what shipped and what a player gets.
/// Accepts wgpu's own spellings: `vulkan`, `gl`, `metal`, `dx12`, `primary`.
pub fn backends_from_env() -> wgpu::Backends {
    match std::env::var("WGPU_BACKEND") {
        Ok(v) if !v.trim().is_empty() => {
            let want = wgpu::Backends::from_comma_list(&v);
            if want.is_empty() {
                eprintln!("WGPU_BACKEND={v:?} names no backend floptle knows — using all of them");
                wgpu::Backends::all()
            } else {
                want
            }
        }
        _ => wgpu::Backends::all(),
    }
}

fn gpu_error(e: &wgpu::Error) {
    let message = describe(e);
    if let Ok(mut g) = GPU_ERRORS.lock() {
        let (queue, seen) = &mut *g;
        if seen.contains(&message) {
            return;
        }
        // Always to the terminal too: somebody running from a shell should see
        // it whether or not a host ever drains the queue.
        eprintln!("GPU error: {message}");
        seen.push(message.clone());
        queue.push(message);
    }
}

/// Take the GPU errors reported since the last call — the editor drains this
/// into the Console each frame. Empty when the GPU is happy, which is the
/// normal case and costs one uncontended lock.
pub fn take_gpu_errors() -> Vec<String> {
    GPU_ERRORS.lock().map(|mut g| std::mem::take(&mut g.0)).unwrap_or_default()
}

/// Owns the GPU connection and (when windowed) the surface. `surface` is `None`
/// for a headless GPU — one created without a window for offscreen rendering
/// (tests, bakes, thumbnails). The passes only ever touch device/queue/config, so
/// they work identically either way.
pub struct Gpu {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub surface: Option<wgpu::Surface<'static>>,
    pub config: wgpu::SurfaceConfiguration,
    depth_tex: wgpu::Texture,
    depth_view: wgpu::TextureView,
    /// The format every SCENE-space pass renders into — the raster, the
    /// raymarch, particles, the 2D light composite, world-space UI, the editor's
    /// grid and gizmo overlays, live render targets, and the post chain's own
    /// scratch. See [`Gpu::scene_format`].
    scene_format: wgpu::TextureFormat,
    /// The present modes this surface actually supports, so a requested one can
    /// fall back rather than fail.
    present_modes: Vec<wgpu::PresentMode>,
    vsync: Vsync,
}

/// How finished frames are handed to the display.
///
/// **This is a setting because `Fifo` is not always what it says it is.** Fifo
/// means "present every frame in order at the monitor's cadence", and when a
/// driver honours that it is the right default: the loop blocks in present, so
/// frame times lock to the refresh and what the simulation sampled matches what
/// reaches the glass. On some compositors it instead presents at a *fraction* of
/// the refresh — a window that does nothing but clear itself blue can sit at a
/// flat 20 fps on a 60 Hz display — and with the mode hardcoded there was
/// nothing a project could do about it but conclude the engine was slow.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Vsync {
    /// `Fifo`. Every frame shown, in order, at the display's cadence. The
    /// default, and the one whose pacing is predictable.
    #[default]
    On,
    /// `Mailbox`: render freely, and the display takes the newest frame at each
    /// refresh. No tearing and no cap — but the frames that reach the glass
    /// sampled the simulation at moments unrelated to when they are shown, which
    /// reads as movement judder that comes and goes with the window mode. Worth
    /// it when `On` is capping you far below what the scene actually costs.
    Adaptive,
    /// `Immediate`: present the instant a frame is ready, tearing and all. What
    /// you want when the question is "how expensive is this frame, really".
    Off,
}

/// The wgpu mode a [`Vsync`] asks for, before availability is considered.
fn wanted_mode(v: Vsync) -> wgpu::PresentMode {
    match v {
        Vsync::On => wgpu::PresentMode::Fifo,
        Vsync::Adaptive => wgpu::PresentMode::Mailbox,
        Vsync::Off => wgpu::PresentMode::Immediate,
    }
}

/// …and what the surface can actually do. `Fifo` is required of every surface by
/// the spec, so it is always a valid answer.
fn pick_present_mode(v: Vsync, available: &[wgpu::PresentMode]) -> wgpu::PresentMode {
    let want = wanted_mode(v);
    if available.contains(&want) { want } else { wgpu::PresentMode::Fifo }
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

/// The timestamp features, when this adapter has them.
///
/// **Asked for, never required.** They are what lets the editor say where a
/// frame's time went (see [`crate::gpu_timer`]), and a device that cannot offer
/// them must still start — an engine that refused to run on a card without a
/// profiler would be trading the product for the tool. `GpuTimer::new` asks the
/// DEVICE afterwards, so the two can never drift apart.
fn timing_features(adapter: &wgpu::Adapter) -> wgpu::Features {
    let want = wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS;
    adapter.features() & want
}

impl Gpu {
    /// Create the GPU connection for `window` and configure its surface. Picks a
    /// high-performance adapter, an sRGB surface format when available, and Mailbox
    /// present mode (low-latency) falling back to Fifo (vsync). Lifted from the
    /// proof's proven wgpu-29 bootstrap.
    pub fn new(window: Arc<Window>) -> Self {
        let size = window.inner_size();

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: backends_from_env(),
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
            required_features: timing_features(&adapter),
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .expect("no GPU device");
        // A GPU validation error must not end the session.
        //
        // wgpu's default handler panics, and a panic here is worse than it
        // sounds: it unwinds through a frame that is holding a surface
        // texture, the swapchain destructor panics in turn, and the process
        // takes a non-unwinding abort — everything unsaved gone, and the crash
        // note naming the destructor rather than the cause. Twice now that has
        // been one mismatched pipeline in a pass that draws once a frame.
        //
        // So: record it, keep the frame, let the editor say so. Deliberately
        // NOT installed on the headless path (see `headless_with`) — a probe
        // that swallowed a validation error would report a pass it never made,
        // and that trade only makes sense when there is a person at the window.
        device.on_uncaptured_error(Arc::new(|e: wgpu::Error| gpu_error(&e)));

        let caps = surface.get_capabilities(&adapter);
        let format =
            caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(caps.formats[0]);
        // Fifo (classic vsync) is the DEFAULT, deliberately — see [`Vsync`] for
        // why, and for why it is no longer the only choice.
        let present_modes = caps.present_modes.clone();
        let vsync = Vsync::default();
        let present_mode = pick_present_mode(vsync, &present_modes);
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

        let (depth_tex, depth_view) = Self::make_depth(&device, config.width, config.height);

        Self {
            instance,
            adapter,
            device,
            queue,
            surface: Some(surface),
            config,
            depth_tex,
            depth_view,
            scene_format: Self::HDR_FORMAT,
            present_modes,
            vsync,
        }
    }

    /// How this GPU is presenting, and how to change it.
    ///
    /// Changing it reconfigures the surface, which is cheap and takes effect on
    /// the next frame. A mode the surface does not support falls back to `Fifo`
    /// rather than failing — every surface supports `Fifo`.
    pub fn vsync(&self) -> Vsync {
        self.vsync
    }

    /// Whether `mode` is actually available here. The Inspector greys out what
    /// this says no to, rather than offering a choice that silently does nothing
    /// — which is how "I turned it off and it did not change" happens.
    pub fn supports_vsync(&self, mode: Vsync) -> bool {
        self.surface.is_some() && self.present_modes.contains(&wanted_mode(mode))
    }

    /// Returns the wgpu mode actually applied — which is not always the one
    /// asked for, since a surface need only support `Fifo`. The caller reports
    /// it, so a setting that quietly did nothing is visible instead of being
    /// mistaken for a setting that did not help.
    pub fn set_vsync(&mut self, mode: Vsync) -> Option<wgpu::PresentMode> {
        if self.vsync == mode || self.surface.is_none() {
            return None;
        }
        self.vsync = mode;
        self.config.present_mode = pick_present_mode(mode, &self.present_modes);
        if let Some(surface) = self.surface.as_ref() {
            surface.configure(&self.device, &self.config);
        }
        Some(self.config.present_mode)
    }

    /// Create a headless GPU (no window/surface) for offscreen rendering at
    /// `width`×`height`. `config` carries the same sRGB format the windowed path
    /// uses, so pipelines built against `surface_format()` render identically; the
    /// caller supplies its own color target (a texture with `COPY_SRC`) to read
    /// back. Used by render tests and tools.
    pub fn headless(width: u32, height: u32) -> Self {
        Self::headless_with(width, height, None)
    }

    /// [`headless`](Self::headless) with the HDR scene format the windowed path
    /// uses — for a probe that means to exercise the pipeline as it SHIPS.
    ///
    /// The plain `headless` deliberately keeps the 8-bit surface format for the
    /// scene as well, because forty render probes read their target back as
    /// tightly-packed RGBA8 and none of them is about the format. This is the
    /// opt-in for the ones that are.
    pub fn headless_hdr(width: u32, height: u32) -> Self {
        Self::headless_with(width, height, Some(Self::HDR_FORMAT))
    }

    fn headless_with(width: u32, height: u32, scene: Option<wgpu::TextureFormat>) -> Self {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: backends_from_env(),
            flags: wgpu::InstanceFlags::default(),
            memory_budget_thresholds: Default::default(),
            backend_options: Default::default(),
            display: None,
        });
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .expect("no GPU adapter (headless)");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("floptle-device-headless"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::default(),
            memory_hints: wgpu::MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        }))
        .expect("no GPU device (headless)");
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            width: width.max(1),
            height: height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
        };
        let (depth_tex, depth_view) = Self::make_depth(&device, config.width, config.height);
        let scene_format = scene.unwrap_or(config.format);
        Self {
            instance,
            adapter,
            device,
            queue,
            surface: None,
            config,
            depth_tex,
            depth_view,
            scene_format,
            // Headless has no surface, so no presentation to configure.
            present_modes: Vec::new(),
            vsync: Vsync::On,
        }
    }

    /// The depth format the renderer uses everywhere (always available as a depth
    /// attachment; matches wgpu's `0..1` reverse-Z-free convention with `Less`).
    pub const DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;

    /// Build a depth target sized to the surface. Recreated on every resize so it
    /// can never desync from the swapchain (a size mismatch is a hard validation
    /// error at draw time). `TEXTURE_BINDING` so post passes (SSAO) can sample it;
    /// `COPY_DST` so the opaque depth prepass can prime it (see `Raster`).
    fn make_depth(device: &wgpu::Device, width: u32, height: u32) -> (wgpu::Texture, wgpu::TextureView) {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("depth"),
            size: wgpu::Extent3d { width: width.max(1), height: height.max(1), depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: Self::DEPTH_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        (texture, view)
    }

    /// The depth view passes attach for depth testing.
    pub fn depth_view(&self) -> &wgpu::TextureView {
        &self.depth_view
    }

    /// The depth TEXTURE behind [`depth_view`](Self::depth_view) — the copy target
    /// when the opaque depth prepass primes the frame's depth buffer.
    pub fn depth_texture(&self) -> &wgpu::Texture {
        &self.depth_tex
    }

    /// The surface's swapchain format — every pass that targets the screen needs it.
    pub fn surface_format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    /// The format every SCENE-space pass renders into, which is **not** the
    /// surface format.
    ///
    /// A window's surface is 8-bit sRGB: it can hold nothing brighter than
    /// white, and anything that goes over gets clipped per channel — which is
    /// not merely "bright things go white", it is a HUE SHIFT, because the
    /// channel that clips first drags the colour toward the other two. A sunlit
    /// white wall and a light bulb ten times brighter store as the same pixel,
    /// so bloom cannot tell them apart and there is nothing left for an exposure
    /// or a tonemap to work with.
    ///
    /// So the scene renders into a floating-point target and stays in linear
    /// light, at whatever intensity it actually has, all the way to the end of
    /// the post chain — where exactly ONE pass maps it down to the display
    /// ([`PostSettings::tonemap`](crate::PostSettings)). Every pass in between —
    /// depth of field, denoise, grade, bloom, lens, sharpen — is then working on
    /// the real values.
    ///
    /// Windowed rendering uses `Rgba16Float`. A headless GPU keeps the surface
    /// format unless it was built by [`headless_hdr`](Self::headless_hdr), so
    /// the render probes go on reading 8-bit RGBA the way they always have.
    pub fn scene_format(&self) -> wgpu::TextureFormat {
        self.scene_format
    }

    /// The scene format the windowed path uses. Half floats rather than full:
    /// a 16-bit float holds ~5 decimal digits and reaches 65504, which is far
    /// past any light anybody sets, at half the bandwidth of `Rgba32Float`.
    pub const HDR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

    /// Reconfigure the surface after the window resizes. Clamps to a minimum of 1
    /// so a minimized window (0×0) doesn't produce an invalid configuration, and
    /// rebuilds the depth target to match.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.config.width = width.max(1);
        self.config.height = height.max(1);
        if let Some(surface) = self.surface.as_ref() {
            surface.configure(&self.device, &self.config);
        }
        let (depth_tex, depth_view) = Self::make_depth(&self.device, self.config.width, self.config.height);
        self.depth_tex = depth_tex;
        self.depth_view = depth_view;
    }

    /// Acquire the next swapchain image to render into. Returns `None` on a
    /// transient surface state (Outdated/Lost — reconfigured here) or failure, in
    /// which case the caller simply skips the frame.
    pub fn acquire(&mut self) -> Option<Frame> {
        use wgpu::CurrentSurfaceTexture as C;
        let surface = self.surface.as_ref()?;
        let surface = match surface.get_current_texture() {
            C::Success(t) | C::Suboptimal(t) => t,
            C::Outdated | C::Lost => {
                surface.configure(&self.device, &self.config);
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
