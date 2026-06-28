//! Floptle — Beat 2 "Stand in the Dream" proof slice.
//!
//! A standalone, hardcoded-WGSL binary (sibling to Beat 1's `main.rs`, which it
//! leaves untouched). It proves the SDF-first physics thesis (ADR-0012 / -0014):
//! a kinematic capsule walks on a *morphing* fractal planetoid, colliding against
//! the renderer's own signed-distance field, with SDF-surface gravity defining
//! "down" so you can run up the shifting walls — and an anti-trapping rule so a
//! heaving surface lifts you instead of swallowing you.
//!
//! The design was vetted by an adversarial panel: the visible crust is a fractal,
//! but the COLLISION field is an explicitly-designed smooth, solid planetoid
//! (core sphere + blended hills), which is genuinely walkable and never empty.
//!
//! Controls: WASD move (camera-relative, on the surface), Space jump, Shift
//! sprint, mouse look (click to capture, Esc release), F cycle third/first
//! person, R respawn above the planet, Esc (uncaptured) quits.

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
const RENDER_DIV: u32 = 1;

// ---- macro field constants (LOCK-STEP with walk.wgsl) ----
const R0: f32 = 16.0;
const KB: f32 = 3.0;
const WARP_A: f32 = 0.8;
const WARP_W: f32 = 1.0;
const WARP_K: f32 = 0.2;
/// Bump (hill) directions + radii; centers sit at normalize(dir)*(R0*0.84).
const BUMPS: [([f32; 3], f32); 6] = [
    ([1.0, 0.0, 0.2], 6.6),
    ([-0.8, 0.3, 0.6], 5.1),
    ([0.2, 1.0, 0.0], 6.0),
    ([0.0, -1.0, 0.3], 4.8),
    ([0.5, 0.2, -1.0], 7.2),
    ([-0.4, -0.5, -0.8], 5.4),
];

// ---- character / physics tuning ----
const CAP_R: f32 = 0.18; // capsule radius
const CAP_HH: f32 = 0.22; // capsule half-height (segment)
const G_MAG: f32 = 14.0; // gravity acceleration
const ACCEL: f32 = 40.0; // tangential input accel
const FRIC: f32 = 8.0; // tangential friction (grounded)
const JUMP_V: f32 = 6.0;
const SLOPE_COS: f32 = 0.64; // cos(~50deg): steeper than this = not grounded
const V_SHOVE_MAX: f32 = 3.0; // clamp on depenetration + surface-carry speed
const V_DEAD: f32 = 0.05; // surface-carry deadband (kills standing jitter)
const SURF_VMAX: f32 = WARP_A * WARP_W * 1.7320508; // analytic |dw/dt| bound
const K_UP: f32 = 10.0; // up-vector temporal smoothing rate
const EPS_N: f32 = 0.03; // sharp eps for contact/depenetration normals
const EPS_G: f32 = 0.10; // coarse eps for the gravity up-vector low-pass
const G_MIN: f32 = 0.3; // |grad| below this => normal is untrustworthy
const SKIN: f32 = 0.01;
const GROUND_EPS: f32 = 0.06;
const BLEND_NEAR: f32 = 0.15; // gravity follows terrain within this of surface
const BLEND_FAR: f32 = 0.8; // gravity is radial beyond this
const COYOTE: f32 = 0.1;
const BOOM: f32 = 3.0; // third-person boom length
const EYE: f32 = 0.15;

// ----------------------------- the field -----------------------------------

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn smin(a: f32, b: f32, k: f32) -> f32 {
    let h = (0.5 + 0.5 * (b - a) / k).clamp(0.0, 1.0);
    (b + (a - b) * h) - k * h * (1.0 - h)
}

fn warp(p: Vec3, t: f32) -> Vec3 {
    WARP_A
        * Vec3::new(
            (WARP_W * t + WARP_K * p.y).sin(),
            (WARP_W * t + WARP_K * p.z).sin(),
            (WARP_W * t + WARP_K * p.x).sin(),
        )
}

/// Closed-form d/dt of the input warp — drives jitter-free surface velocity.
fn dwarp_dt(p: Vec3, t: f32) -> Vec3 {
    WARP_A
        * WARP_W
        * Vec3::new(
            (WARP_W * t + WARP_K * p.y).cos(),
            (WARP_W * t + WARP_K * p.z).cos(),
            (WARP_W * t + WARP_K * p.x).cos(),
        )
}

fn f_macro(q: Vec3) -> f32 {
    let mut d = q.length() - R0;
    for (dir, r) in BUMPS {
        let c = Vec3::from_array(dir).normalize() * (R0 * 0.84);
        d = smin(d, (q - c).length() - r, KB);
    }
    d
}

/// Collision field: the smooth, solid macro planetoid (NO crust). Source of
/// truth for all physics.
fn f_c(p: Vec3, t: f32) -> f32 {
    f_macro(p + warp(p, t))
}

/// Central-difference gradient, normalized by 2*eps so |grad| ~ 1 on a metric
/// field. `eps` is decoupled: sharp (EPS_N) for contact, coarse (EPS_G) for up.
fn grad(p: Vec3, t: f32, eps: f32) -> Vec3 {
    let dx = f_c(p + Vec3::new(eps, 0.0, 0.0), t) - f_c(p - Vec3::new(eps, 0.0, 0.0), t);
    let dy = f_c(p + Vec3::new(0.0, eps, 0.0), t) - f_c(p - Vec3::new(0.0, eps, 0.0), t);
    let dz = f_c(p + Vec3::new(0.0, 0.0, eps), t) - f_c(p - Vec3::new(0.0, 0.0, eps), t);
    Vec3::new(dx, dy, dz) / (2.0 * eps)
}

/// Gravity "down" = -(blend of radial backbone and terrain normal). Radial when
/// far or where the gradient is weak (never degenerate); follows the wall near
/// the surface (so you can run up it — ADR-0014).
fn gravity_down(p: Vec3, t: f32) -> Vec3 {
    let f = f_c(p, t);
    let g = grad(p, t, EPS_G);
    let gm = g.length();
    let radial = p.try_normalize().unwrap_or(Vec3::Y);
    let terrain = if gm > 1e-5 { g / gm } else { radial };
    let w = smoothstep(BLEND_FAR, BLEND_NEAR, f) * smoothstep(G_MIN, 0.5, gm);
    let up = radial.lerp(terrain, w).try_normalize().unwrap_or(radial);
    -up
}

// --------------------------- the controller ---------------------------------

struct Character {
    pos: Vec3,
    vel: Vec3,
    up_smooth: Vec3,
    grounded: bool,
    coyote: f32,
    // telemetry for the HUD + contact ring
    contact: Vec3,
    f_player: f32,
    v_surface: f32,
}

impl Character {
    fn spawn() -> Self {
        let dir = Vec3::new(0.3, 1.0, 0.4).normalize();
        let pos = dir * (R0 + 6.0);
        Character {
            pos,
            vel: Vec3::ZERO,
            up_smooth: dir,
            grounded: false,
            coyote: 0.0,
            contact: pos,
            f_player: 0.0,
            v_surface: 0.0,
        }
    }

    fn step(&mut self, dt: f32, time: f32, wish: Vec3, sprint: bool, jump: bool) {
        let dt = dt.min(0.033);

        // jump uses last frame's grounded + coyote window
        self.coyote = if self.grounded { COYOTE } else { (self.coyote - dt).max(0.0) };
        if jump && (self.grounded || self.coyote > 0.0) {
            self.vel += self.up_smooth * JUMP_V;
            self.grounded = false;
            self.coyote = 0.0;
        }

        // substep count sized off BOTH travel speed AND surface speed (no tunneling)
        let speed = self.vel.length().max(SURF_VMAX);
        let n = ((speed * dt) / (0.5 * CAP_R)).ceil().max(4.0) as u32;
        let sub = dt / n as f32;

        for _ in 0..n {
            let up = self.up_smooth;

            // (1) SURFACE-VELOCITY CARRY — analytic df/dt = grad . dw/dt, so a
            //     rising wall lifts the rider instead of swallowing them.
            let gn = grad(self.pos, time, EPS_N);
            let gm = gn.length();
            if gm > G_MIN {
                let nrm = gn / gm;
                let mut vsurf = -nrm.dot(dwarp_dt(self.pos, time));
                vsurf = vsurf.clamp(-V_SHOVE_MAX, V_SHOVE_MAX);
                self.v_surface = vsurf;
                if vsurf.abs() > V_DEAD {
                    self.pos += nrm * vsurf * sub;
                    // ride in velocity space too (kills the slam-back when the wall stops)
                    if self.grounded {
                        let vn = self.vel.dot(nrm);
                        self.vel += nrm * (vsurf - vn);
                    }
                }
            } else {
                self.v_surface = 0.0;
            }

            // (2) GRAVITY
            let gdir = gravity_down(self.pos, time);
            self.vel += gdir * G_MAG * sub;

            // (3) INPUT (tangential) + friction
            if wish.length_squared() > 1e-6 {
                let s = if sprint { 1.8 } else { 1.0 };
                self.vel += wish.normalize() * ACCEL * s * sub;
            }
            if self.grounded {
                let v_n = up * self.vel.dot(up);
                let v_t = self.vel - v_n;
                self.vel = v_n + v_t * (-FRIC * sub).exp();
            }

            // (4) INTEGRATE (substep keeps the move < 0.5r, so no tunneling)
            self.pos += self.vel * sub;

            // (5) DEPENETRATION — 3 spheres along the segment; position only,
            //     never momentum; clamped so a fast morph nudges, never launches.
            let max_shove = V_SHOVE_MAX * sub;
            let caps = [
                self.pos - up * CAP_HH,
                self.pos,
                self.pos + up * CAP_HH,
            ];
            let mut correction = Vec3::ZERO;
            let mut deepest_f = f32::INFINITY;
            let mut contact_n = up;
            for cap in caps {
                let mut c = cap;
                for _ in 0..4 {
                    let f = f_c(c, time);
                    if f >= CAP_R - SKIN {
                        break;
                    }
                    let g = grad(c, time, EPS_N);
                    let gm = g.length();
                    let nrm = if gm > G_MIN { g / gm } else { c.try_normalize().unwrap_or(up) };
                    c += nrm * (CAP_R - f).min(max_shove);
                }
                correction += c - cap;
                let f0 = f_c(cap, time);
                if f0 < deepest_f {
                    deepest_f = f0;
                    contact_n = grad(cap, time, EPS_N).try_normalize().unwrap_or(up);
                }
            }
            self.pos += correction / 3.0;

            // (6) SLIDE — kill into-surface velocity
            if deepest_f < CAP_R + 0.02 {
                let into = self.vel.dot(contact_n).min(0.0);
                self.vel -= contact_n * into;
            }

            // (7) GROUNDED + up target
            let lo = self.pos - up * CAP_HH;
            let f_lo = f_c(lo, time);
            let n_lo = grad(lo, time, EPS_N).try_normalize().unwrap_or(up);
            self.grounded = f_lo <= CAP_R + GROUND_EPS && n_lo.dot(up) > SLOPE_COS;

            let up_target = if self.grounded { n_lo } else { -gravity_down(self.pos, time) };
            let a = 1.0 - (-K_UP * sub).exp();
            self.up_smooth = self.up_smooth.lerp(up_target, a).try_normalize().unwrap_or(up);

            // telemetry
            self.contact = lo - n_lo * f_lo;
            self.f_player = f_lo;
        }
    }
}

// ------------------------------ rendering -----------------------------------

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
    capsule_pos: [f32; 4],
    capsule_up: [f32; 4],
    contact: [f32; 4],
}

#[derive(Default)]
struct Input {
    w: bool,
    a: bool,
    s: bool,
    d: bool,
    jump: bool,
    sprint: bool,
    captured: bool,
    mouse_dx: f32,
    mouse_dy: f32,
}

struct Targets {
    scene_view: TextureView,
    hist_view: [TextureView; 2],
    bg_post: [BindGroup; 2],
    bg_present: [BindGroup; 2],
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
    cc: Character,
    cam_yaw: f32,
    cam_pitch: f32,
    cam_mode: u32, // 0 = third person, 1 = first person
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
        let format = caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(caps.formats[0]);
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
            make_pipeline(&device, &[Some(&bgl_globals)], include_str!("../walk.wgsl"), "walk", HDR);
        let post_pl = make_pipeline(
            &device,
            &[Some(&bgl_globals), Some(&bgl_post)],
            include_str!("../post.wgsl"),
            "post",
            HDR,
        );
        let present_pl = make_pipeline(
            &device,
            &[Some(&bgl_globals), Some(&bgl_present)],
            include_str!("../present.wgsl"),
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

        let render_size = (config.width.max(2) / RENDER_DIV, config.height.max(2) / RENDER_DIV);
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
            cc: Character::spawn(),
            cam_yaw: 0.0,
            cam_pitch: 0.25,
            cam_mode: 0,
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
        self.render_size = (w.max(2) / RENDER_DIV, h.max(2) / RENDER_DIV);
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

    /// Stable tangent-plane basis from the smoothed up + camera yaw.
    /// Returns (up, forward_tangent, right_tangent).
    fn tangent_basis(&self) -> (Vec3, Vec3, Vec3) {
        let up = self.cc.up_smooth;
        let world_ref = if up.dot(Vec3::Z).abs() < 0.9 { Vec3::Z } else { Vec3::X };
        let f0 = (world_ref - up * world_ref.dot(up)).try_normalize().unwrap_or(Vec3::X);
        let r0 = up.cross(f0).normalize();
        let (sy, cy) = self.cam_yaw.sin_cos();
        let fwd_t = (f0 * cy + r0 * sy).try_normalize().unwrap_or(f0);
        let right_t = fwd_t.cross(up).try_normalize().unwrap_or(r0);
        (up, fwd_t, right_t)
    }

    fn update(&mut self, dt: f32, time: f32) {
        // mouse look
        let sens = 0.0025;
        self.cam_yaw += self.input.mouse_dx * sens;
        self.cam_pitch = (self.cam_pitch - self.input.mouse_dy * sens).clamp(-1.0, 1.2);
        self.input.mouse_dx = 0.0;
        self.input.mouse_dy = 0.0;

        // camera-relative move on the tangent plane
        let (_up, fwd_t, right_t) = self.tangent_basis();
        let mut wish = Vec3::ZERO;
        if self.input.w {
            wish += fwd_t;
        }
        if self.input.s {
            wish -= fwd_t;
        }
        if self.input.d {
            wish += right_t;
        }
        if self.input.a {
            wish -= right_t;
        }

        self.cc.step(dt, time, wish, self.input.sprint, self.input.jump);
        self.input.jump = false;
    }

    fn camera(&self, time: f32) -> ([f32; 4], [f32; 4], [f32; 4], [f32; 4]) {
        let (up, fwd_t, _right_t) = self.tangent_basis();
        let (cam_pos, fwd) = if self.cam_mode == 1 {
            // first person: look with pitch (positive = up)
            let (sp, cp) = self.cam_pitch.sin_cos();
            let look = (fwd_t * cp + up * sp).try_normalize().unwrap_or(fwd_t);
            (self.cc.pos + up * EYE, look)
        } else {
            // third person: orbit ABOVE and BEHIND, always looking down at the
            // player. The elevation is clamped so the camera can never dip under
            // the horizon (which is what made the view feel inverted).
            let target = self.cc.pos + up * (CAP_HH + 0.3);
            let e = (0.55 - self.cam_pitch * 0.5).clamp(0.15, 1.3);
            let (se, ce) = e.sin_cos();
            let dir_to_cam = (-fwd_t * ce + up * se).try_normalize().unwrap_or(up);
            // spring-arm: pull in if the boom would clip the planet
            let dist: f32;
            let mut s = 0.4_f32;
            loop {
                let d = f_c(target + dir_to_cam * s, time) - 0.2;
                if d < 0.0 {
                    dist = s.max(0.5);
                    break;
                }
                s += d.max(0.08);
                if s >= BOOM {
                    dist = BOOM;
                    break;
                }
            }
            let cp_pos = target + dir_to_cam * dist;
            (cp_pos, (target - cp_pos).try_normalize().unwrap_or(-fwd_t))
        };
        let right = fwd.cross(up).try_normalize().unwrap_or(Vec3::X);
        let camup = right.cross(fwd).normalize();
        (
            [cam_pos.x, cam_pos.y, cam_pos.z, 0.0],
            [right.x, right.y, right.z, 0.0],
            [camup.x, camup.y, camup.z, 0.0],
            [fwd.x, fwd.y, fwd.z, 0.0],
        )
    }

    fn render(&mut self) {
        let now = Instant::now();
        let dt = (now - self.last).as_secs_f32().min(0.1);
        self.last = now;
        let time = (now - self.start).as_secs_f32();
        self.update(dt, time);

        let (cam_pos, cam_right, cam_up, cam_fwd) = self.camera(time);
        let up = self.cc.up_smooth;
        let g = Globals {
            cam_pos,
            cam_right,
            cam_up,
            cam_fwd,
            resolution: [self.render_size.0 as f32, self.render_size.1 as f32],
            time,
            dt,
            frame: self.frame as f32,
            feedback: 0.5,
            warp: 1.0,
            fov: 1.25,
            capsule_pos: [self.cc.pos.x, self.cc.pos.y, self.cc.pos.z, CAP_R],
            capsule_up: [up.x, up.y, up.z, CAP_HH],
            contact: [
                self.cc.contact.x,
                self.cc.contact.y,
                self.cc.contact.z,
                if self.cc.grounded { 1.0 } else { 0.0 },
            ],
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

        {
            let mut rp = enc.begin_render_pass(&RenderPassDescriptor {
                label: Some("walk"),
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

        self.fps_frames += 1;
        let since = now - self.fps_t;
        if since.as_secs_f32() >= 0.5 {
            let fps = self.fps_frames as f32 / since.as_secs_f32();
            let mode = if self.cam_mode == 1 { "1st" } else { "3rd" };
            self.window.set_title(&format!(
                "Floptle — Stand in the Dream (Beat 2)  |  {fps:.0} fps  [{mode}]  grounded:{}  f:{:+.2}  vsurf:{:+.2}",
                self.cc.grounded as u8, self.cc.f_player, self.cc.v_surface
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
            .with_title("Floptle — Stand in the Dream (Beat 2)")
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        self.state = Some(State::new(window));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
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
                        KeyCode::Space => state.input.jump = state.input.jump || pressed,
                        KeyCode::ShiftLeft | KeyCode::ShiftRight => state.input.sprint = pressed,
                        KeyCode::KeyF if pressed => state.cam_mode = (state.cam_mode + 1) % 2,
                        KeyCode::KeyR if pressed => state.cc = Character::spawn(),
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
