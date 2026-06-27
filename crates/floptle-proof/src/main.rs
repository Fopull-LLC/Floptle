//! Floptle — Beat 1 "Am I Dreaming?" proof slice.
//!
//! A standalone, hardcoded-WGSL binary (NO engine: no ECS/RON/shader-IR/gravity).
//! It flies a free camera through a time-morphing Mandelbox raymarched at half
//! resolution into an HDR target, runs a feedback/swirl post pass for melting
//! dream-trails, and upscales with tonemap + chromatic aberration + vignette +
//! dither in the present pass. FPS / frame-time are shown in the title bar.
//!
//! Controls: WASD move, Q/E down/up, mouse-look (click to capture, Esc release),
//! arrow keys also look, Shift = boost, R = reset camera, Esc (uncaptured) quits.

use std::sync::Arc;
use std::time::Instant;

use glam::Vec3;
use wgpu::*;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

const HDR: TextureFormat = TextureFormat::Rgba16Float;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Globals {
    cam_pos: [f32; 4],
    cam_right: [f32; 4],
    cam_up: [f32; 4],
    cam_fwd: [f32; 4],
    resolution: [f32; 2],
    time: f32,
    dt: f32,
    frame: f32,
    feedback: f32,
    warp: f32,
    fov: f32,
}

struct Camera {
    pos: Vec3,
    yaw: f32,
    pitch: f32,
    fov: f32,
}

impl Camera {
    fn reset() -> Self {
        Camera { pos: Vec3::new(0.0, 0.0, 5.0), yaw: 0.0, pitch: 0.0, fov: 1.25 }
    }
    /// (forward, right, up)
    fn basis(&self) -> (Vec3, Vec3, Vec3) {
        let (sy, cy) = self.yaw.sin_cos();
        let (sp, cp) = self.pitch.sin_cos();
        let fwd = Vec3::new(cp * sy, sp, -cp * cy).normalize();
        let right = fwd.cross(Vec3::Y).normalize();
        let up = right.cross(fwd).normalize();
        (fwd, right, up)
    }
}

#[derive(Default)]
struct Input {
    w: bool,
    a: bool,
    s: bool,
    d: bool,
    q: bool,
    e: bool,
    up: bool,
    down: bool,
    left: bool,
    right: bool,
    boost: bool,
    captured: bool,
    mouse_dx: f32,
    mouse_dy: f32,
}

struct Targets {
    scene_view: TextureView,
    hist_view: [TextureView; 2],
    bg_post: [BindGroup; 2],    // reads scene + hist[i]
    bg_present: [BindGroup; 2], // reads hist[i]
}

fn build_targets(
    device: &Device,
    queue: &Queue,
    bgl_post: &BindGroupLayout,
    bgl_present: &BindGroupLayout,
    sampler: &Sampler,
    w: u32,
    h: u32,
) -> Targets {
    let mk = |label: &str| {
        device
            .create_texture(&TextureDescriptor {
                label: Some(label),
                size: Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: TextureDimension::D2,
                format: HDR,
                usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&TextureViewDescriptor::default())
    };

    let scene_view = mk("scene");
    let hist0 = mk("hist0");
    let hist1 = mk("hist1");

    let post = |hist: &TextureView| {
        device.create_bind_group(&BindGroupDescriptor {
            label: Some("bg_post"),
            layout: bgl_post,
            entries: &[
                BindGroupEntry { binding: 0, resource: BindingResource::TextureView(&scene_view) },
                BindGroupEntry { binding: 1, resource: BindingResource::Sampler(sampler) },
                BindGroupEntry { binding: 2, resource: BindingResource::TextureView(hist) },
            ],
        })
    };
    let present = |hist: &TextureView| {
        device.create_bind_group(&BindGroupDescriptor {
            label: Some("bg_present"),
            layout: bgl_present,
            entries: &[
                BindGroupEntry { binding: 0, resource: BindingResource::TextureView(hist) },
                BindGroupEntry { binding: 1, resource: BindingResource::Sampler(sampler) },
            ],
        })
    };

    let bg_post = [post(&hist0), post(&hist1)];
    let bg_present = [present(&hist0), present(&hist1)];

    // Clear scene + both history targets so the first feedback read is black.
    let mut enc = device.create_command_encoder(&CommandEncoderDescriptor { label: Some("clear") });
    for v in [&scene_view, &hist0, &hist1] {
        enc.begin_render_pass(&RenderPassDescriptor {
            label: Some("clear"),
            color_attachments: &[Some(RenderPassColorAttachment {
                view: v,
                depth_slice: None,
                resolve_target: None,
                ops: Operations { load: LoadOp::Clear(Color::BLACK), store: StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
    }
    queue.submit(std::iter::once(enc.finish()));

    Targets { scene_view, hist_view: [hist0, hist1], bg_post, bg_present }
}

fn make_pipeline(
    device: &Device,
    layouts: &[Option<&BindGroupLayout>],
    src: &str,
    label: &str,
    fmt: TextureFormat,
) -> RenderPipeline {
    let sm = device.create_shader_module(ShaderModuleDescriptor {
        label: Some(label),
        source: ShaderSource::Wgsl(src.into()),
    });
    let layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: layouts,
        immediate_size: 0,
    });
    device.create_render_pipeline(&RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&layout),
        vertex: VertexState {
            module: &sm,
            entry_point: Some("vs"),
            compilation_options: PipelineCompilationOptions::default(),
            buffers: &[],
        },
        primitive: PrimitiveState::default(),
        depth_stencil: None,
        multisample: MultisampleState::default(),
        fragment: Some(FragmentState {
            module: &sm,
            entry_point: Some("fs"),
            compilation_options: PipelineCompilationOptions::default(),
            targets: &[Some(ColorTargetState {
                format: fmt,
                blend: None,
                write_mask: ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

struct State {
    window: Arc<Window>,
    surface: Surface<'static>,
    device: Device,
    queue: Queue,
    config: SurfaceConfiguration,
    globals_buf: Buffer,
    bg_globals: BindGroup,
    bgl_post: BindGroupLayout,
    bgl_present: BindGroupLayout,
    sampler: Sampler,
    raymarch_pl: RenderPipeline,
    post_pl: RenderPipeline,
    present_pl: RenderPipeline,
    targets: Targets,
    render_size: (u32, u32),
    cam: Camera,
    input: Input,
    frame: u64,
    start: Instant,
    last: Instant,
    fps_t: Instant,
    fps_frames: u32,
}

impl State {
    fn new(window: Arc<Window>) -> State {
        let size = window.inner_size();
        let instance = Instance::new(InstanceDescriptor {
            backends: Backends::all(),
            flags: InstanceFlags::default(),
            memory_budget_thresholds: Default::default(),
            backend_options: Default::default(),
            display: None,
        });
        let surface = instance.create_surface(window.clone()).expect("create surface");
        let adapter = pollster::block_on(instance.request_adapter(&RequestAdapterOptions {
            power_preference: PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .expect("no adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(&DeviceDescriptor {
            label: Some("device"),
            required_features: Features::empty(),
            required_limits: Limits::default(),
            experimental_features: ExperimentalFeatures::default(),
            memory_hints: MemoryHints::Performance,
            trace: Trace::Off,
        }))
        .expect("no device");

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let present_mode = if caps.present_modes.contains(&PresentMode::Mailbox) {
            PresentMode::Mailbox
        } else {
            PresentMode::Fifo
        };
        let config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let bgl_globals = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("globals"),
            entries: &[BindGroupLayoutEntry {
                binding: 0,
                visibility: ShaderStages::VERTEX_FRAGMENT,
                ty: BindingType::Buffer {
                    ty: BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let tex_entry = |binding: u32| BindGroupLayoutEntry {
            binding,
            visibility: ShaderStages::FRAGMENT,
            ty: BindingType::Texture {
                sample_type: TextureSampleType::Float { filterable: true },
                view_dimension: TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let samp_entry = |binding: u32| BindGroupLayoutEntry {
            binding,
            visibility: ShaderStages::FRAGMENT,
            ty: BindingType::Sampler(SamplerBindingType::Filtering),
            count: None,
        };
        let bgl_post = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("post"),
            entries: &[tex_entry(0), samp_entry(1), tex_entry(2)],
        });
        let bgl_present = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("present"),
            entries: &[tex_entry(0), samp_entry(1)],
        });

        let raymarch_pl =
            make_pipeline(&device, &[Some(&bgl_globals)], include_str!("raymarch.wgsl"), "raymarch", HDR);
        let post_pl = make_pipeline(
            &device,
            &[Some(&bgl_globals), Some(&bgl_post)],
            include_str!("post.wgsl"),
            "post",
            HDR,
        );
        let present_pl = make_pipeline(
            &device,
            &[Some(&bgl_globals), Some(&bgl_present)],
            include_str!("present.wgsl"),
            "present",
            config.format,
        );

        let globals_buf = device.create_buffer(&BufferDescriptor {
            label: Some("globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bg_globals = device.create_bind_group(&BindGroupDescriptor {
            label: Some("bg_globals"),
            layout: &bgl_globals,
            entries: &[BindGroupEntry { binding: 0, resource: globals_buf.as_entire_binding() }],
        });

        let sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("sampler"),
            address_mode_u: AddressMode::ClampToEdge,
            address_mode_v: AddressMode::ClampToEdge,
            address_mode_w: AddressMode::ClampToEdge,
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            mipmap_filter: MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let render_size = (config.width.max(2) / 2, config.height.max(2) / 2);
        let targets =
            build_targets(&device, &queue, &bgl_post, &bgl_present, &sampler, render_size.0, render_size.1);

        let now = Instant::now();
        State {
            window,
            surface,
            device,
            queue,
            config,
            globals_buf,
            bg_globals,
            bgl_post,
            bgl_present,
            sampler,
            raymarch_pl,
            post_pl,
            present_pl,
            targets,
            render_size,
            cam: Camera::reset(),
            input: Input::default(),
            frame: 0,
            start: now,
            last: now,
            fps_t: now,
            fps_frames: 0,
        }
    }

    fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(&self.device, &self.config);
        self.render_size = (w.max(2) / 2, h.max(2) / 2);
        self.targets = build_targets(
            &self.device,
            &self.queue,
            &self.bgl_post,
            &self.bgl_present,
            &self.sampler,
            self.render_size.0,
            self.render_size.1,
        );
    }

    fn update(&mut self, dt: f32) {
        let (fwd, right, up) = self.cam.basis();
        let mut mv = Vec3::ZERO;
        if self.input.w { mv += fwd; }
        if self.input.s { mv -= fwd; }
        if self.input.d { mv += right; }
        if self.input.a { mv -= right; }
        if self.input.e { mv += up; }
        if self.input.q { mv -= up; }
        if mv.length_squared() > 0.0 {
            let speed = if self.input.boost { 7.0 } else { 2.2 };
            self.cam.pos += mv.normalize() * speed * dt;
        }

        let sens = 0.0025;
        self.cam.yaw += self.input.mouse_dx * sens;
        self.cam.pitch -= self.input.mouse_dy * sens;
        let ks = 1.4 * dt;
        if self.input.left { self.cam.yaw -= ks; }
        if self.input.right { self.cam.yaw += ks; }
        if self.input.up { self.cam.pitch += ks; }
        if self.input.down { self.cam.pitch -= ks; }
        self.cam.pitch = self.cam.pitch.clamp(-1.55, 1.55);
        self.input.mouse_dx = 0.0;
        self.input.mouse_dy = 0.0;
    }

    fn render(&mut self) {
        let now = Instant::now();
        let dt = (now - self.last).as_secs_f32().min(0.1);
        self.last = now;
        self.update(dt);

        let (fwd, right, up) = self.cam.basis();
        let cp = self.cam.pos;
        let g = Globals {
            cam_pos: [cp.x, cp.y, cp.z, 0.0],
            cam_right: [right.x, right.y, right.z, 0.0],
            cam_up: [up.x, up.y, up.z, 0.0],
            cam_fwd: [fwd.x, fwd.y, fwd.z, 0.0],
            resolution: [self.render_size.0 as f32, self.render_size.1 as f32],
            time: (now - self.start).as_secs_f32(),
            dt,
            frame: self.frame as f32,
            feedback: 0.9,
            warp: 1.0,
            fov: self.cam.fov,
        };
        self.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&g));

        let surface_texture = match self.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(t) | CurrentSurfaceTexture::Suboptimal(t) => t,
            CurrentSurfaceTexture::Outdated | CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            _ => return,
        };
        let surf_view = surface_texture.texture.create_view(&TextureViewDescriptor::default());

        let read = (self.frame % 2) as usize;
        let write = 1 - read;

        let mut enc =
            self.device.create_command_encoder(&CommandEncoderDescriptor { label: Some("frame") });

        // 1) raymarch -> scene
        {
            let mut rp = enc.begin_render_pass(&RenderPassDescriptor {
                label: Some("raymarch"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &self.targets.scene_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations { load: LoadOp::Clear(Color::BLACK), store: StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.raymarch_pl);
            rp.set_bind_group(0, &self.bg_globals, &[]);
            rp.draw(0..3, 0..1);
        }

        // 2) post (feedback) -> hist[write]
        {
            let mut rp = enc.begin_render_pass(&RenderPassDescriptor {
                label: Some("post"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &self.targets.hist_view[write],
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations { load: LoadOp::Clear(Color::BLACK), store: StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.post_pl);
            rp.set_bind_group(0, &self.bg_globals, &[]);
            rp.set_bind_group(1, &self.targets.bg_post[read], &[]);
            rp.draw(0..3, 0..1);
        }

        // 3) present -> swapchain (upscale the fresh composite hist[write])
        {
            let mut rp = enc.begin_render_pass(&RenderPassDescriptor {
                label: Some("present"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &surf_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations { load: LoadOp::Clear(Color::BLACK), store: StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.present_pl);
            rp.set_bind_group(0, &self.bg_globals, &[]);
            rp.set_bind_group(1, &self.targets.bg_present[write], &[]);
            rp.draw(0..3, 0..1);
        }

        self.queue.submit(std::iter::once(enc.finish()));
        surface_texture.present();
        self.frame += 1;

        // Title-bar profiler (CPU frame-time + FPS).
        self.fps_frames += 1;
        let since = now - self.fps_t;
        if since.as_secs_f32() >= 0.5 {
            let fps = self.fps_frames as f32 / since.as_secs_f32();
            let ms = since.as_secs_f32() * 1000.0 / self.fps_frames as f32;
            self.window.set_title(&format!(
                "Floptle — Am I Dreaming? (Beat 1)   |   {fps:.0} fps  {ms:.2} ms"
            ));
            self.fps_frames = 0;
            self.fps_t = now;
        }
    }
}

#[derive(Default)]
struct App {
    state: Option<State>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Floptle — Am I Dreaming? (Beat 1)")
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        self.state = Some(State::new(window));
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id: WindowId,
        event: WindowEvent,
    ) {
        let Some(state) = self.state.as_mut() else { return };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => state.resize(size.width, size.height),
            WindowEvent::RedrawRequested => state.render(),
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                let w = &state.window;
                let _ = w
                    .set_cursor_grab(CursorGrabMode::Locked)
                    .or_else(|_| w.set_cursor_grab(CursorGrabMode::Confined));
                w.set_cursor_visible(false);
                state.input.captured = true;
            }
            WindowEvent::Focused(false) => {
                let _ = state.window.set_cursor_grab(CursorGrabMode::None);
                state.window.set_cursor_visible(true);
                state.input.captured = false;
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                if let PhysicalKey::Code(code) = event.physical_key {
                    match code {
                        KeyCode::KeyW => state.input.w = pressed,
                        KeyCode::KeyA => state.input.a = pressed,
                        KeyCode::KeyS => state.input.s = pressed,
                        KeyCode::KeyD => state.input.d = pressed,
                        KeyCode::KeyQ => state.input.q = pressed,
                        KeyCode::KeyE => state.input.e = pressed,
                        KeyCode::ArrowUp => state.input.up = pressed,
                        KeyCode::ArrowDown => state.input.down = pressed,
                        KeyCode::ArrowLeft => state.input.left = pressed,
                        KeyCode::ArrowRight => state.input.right = pressed,
                        KeyCode::ShiftLeft | KeyCode::ShiftRight => state.input.boost = pressed,
                        KeyCode::KeyR if pressed => state.cam = Camera::reset(),
                        KeyCode::Escape if pressed => {
                            if state.input.captured {
                                let _ = state.window.set_cursor_grab(CursorGrabMode::None);
                                state.window.set_cursor_visible(true);
                                state.input.captured = false;
                            } else {
                                event_loop.exit();
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event {
            if let Some(state) = self.state.as_mut() {
                if state.input.captured {
                    state.input.mouse_dx += delta.0 as f32;
                    state.input.mouse_dy += delta.1 as f32;
                }
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(state) = self.state.as_ref() {
            state.window.request_redraw();
        }
    }
}

fn main() {
    env_logger::init();
    let event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App::default();
    event_loop.run_app(&mut app).expect("run app");
}
