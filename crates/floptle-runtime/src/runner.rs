//! The windowed host: a winit event loop that owns the [`App`], opens a window,
//! creates the GPU, and drives `App::frame` on every redraw — the real core loop
//! made visible.
//!
//! It renders the Phase-2 scene: procedurally-generated **lit, depth-tested meshes**
//! (a spinning cube, a still sphere, a counter-spinning cube) viewed through a
//! **free-fly camera** (RMB-drag look, WASD move, Space/Ctrl up/down), with **FPS in
//! the title bar**. Each object uploads a camera-relative model matrix
//! (`Transform::render_matrix`, ADR-0015) into the instanced forward pass.

use std::sync::Arc;
use std::time::Instant;

use floptle_core::transform::Transform;
use floptle_core::Entity;
use floptle_render::{cube, instance_of, uv_sphere, Globals, Gpu, InstanceRaw, MeshId, Raster};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

use crate::app::{App, Renderable, Shape};
use crate::camera::{FlyCamera, Input};

/// Open a window and run the engine until it's closed. Blocks.
pub fn run() {
    let event_loop = EventLoop::new().expect("create event loop");
    event_loop.set_control_flow(ControlFlow::Poll); // render continuously
    let mut runner = Runner::default();
    event_loop.run_app(&mut runner).expect("run event loop");
}

#[derive(Default)]
struct Runner {
    app: App,
    camera: FlyCamera,
    input: Input,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    raster: Option<Raster>,
    /// Registered mesh handles, indexed by `Shape::index()`.
    mesh_ids: Vec<MeshId>,
    clock: Option<Clock>,
}

struct Clock {
    last: Instant,
    fps_since: Instant,
    fps_frames: u32,
}

impl ApplicationHandler for Runner {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return; // already initialized (e.g. on Android resume)
        }
        let attrs = Window::default_attributes()
            .with_title("Floptle — Phase 2   |   RMB-drag: look   ·   WASD: move")
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        let gpu = Gpu::new(window.clone());
        let mut raster = Raster::new(&gpu);
        // Registration order defines the Shape→MeshId mapping (Shape::index).
        let cube_id = raster.register(&gpu, &cube(0.7));
        let sphere_id = raster.register(&gpu, &uv_sphere(0.85, 24, 36));
        self.mesh_ids = vec![cube_id, sphere_id];
        self.raster = Some(raster);
        self.gpu = Some(gpu);
        let now = Instant::now();
        self.clock = Some(Clock { last: now, fps_since: now, fps_frames: 0 });
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.resize(size.width, size.height);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                if let PhysicalKey::Code(code) = event.physical_key {
                    match code {
                        KeyCode::Escape if pressed => event_loop.exit(),
                        KeyCode::KeyW => self.input.forward = pressed,
                        KeyCode::KeyS => self.input.back = pressed,
                        KeyCode::KeyA => self.input.left = pressed,
                        KeyCode::KeyD => self.input.right = pressed,
                        KeyCode::Space => self.input.up = pressed,
                        KeyCode::ControlLeft => self.input.down = pressed,
                        KeyCode::ShiftLeft => self.input.boost = pressed,
                        _ => {}
                    }
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Right, .. } => {
                let looking = state == ElementState::Pressed;
                self.input.looking = looking;
                if let Some(window) = self.window.as_ref() {
                    if looking {
                        // Confine first (widely supported on X11/Wayland); fall back
                        // to Locked. Either keeps the pointer in-window for look.
                        let _ = window
                            .set_cursor_grab(CursorGrabMode::Confined)
                            .or_else(|_| window.set_cursor_grab(CursorGrabMode::Locked));
                        window.set_cursor_visible(false);
                    } else {
                        let _ = window.set_cursor_grab(CursorGrabMode::None);
                        window.set_cursor_visible(true);
                    }
                }
            }
            WindowEvent::RedrawRequested => self.render(),
            _ => {}
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event {
            if self.input.looking {
                self.camera.look(delta.0 as f32, delta.1 as f32);
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }
}

impl Runner {
    fn render(&mut self) {
        let (Some(gpu), Some(raster), Some(window), Some(clock)) = (
            self.gpu.as_mut(),
            self.raster.as_mut(),
            self.window.as_ref(),
            self.clock.as_mut(),
        ) else {
            return;
        };

        // advance the real core loop with the measured wall-clock delta
        let now = Instant::now();
        let dt = (now - clock.last).as_secs_f32();
        clock.last = now;
        self.camera.update(&self.input, dt);
        self.app.frame(dt); // spins the meshes

        // FPS in the title bar — the day-one profiler (roadmap Phase 1)
        clock.fps_frames += 1;
        let since = now.duration_since(clock.fps_since).as_secs_f32();
        if since >= 0.5 {
            let fps = clock.fps_frames as f32 / since;
            window.set_title(&format!(
                "Floptle — Phase 2   |   {fps:.0} fps   |   RMB-drag: look   ·   WASD: move   ·   Space/Ctrl: up/down"
            ));
            clock.fps_frames = 0;
            clock.fps_since = now;
        }

        // camera view·projection + a single directional light → frame globals
        let aspect = gpu.config.width as f32 / gpu.config.height.max(1) as f32;
        let cam = self.camera.render_camera();
        let view_proj = cam.view_proj(aspect);
        let light = floptle_core::math::Vec3::new(0.4, 0.9, 0.45).normalize();
        let globals = Globals {
            view_proj: view_proj.to_cols_array_2d(),
            light_dir: [light.x, light.y, light.z, 0.0],
            light_color: [1.0, 0.98, 0.92, 0.0],
            ambient: [0.12, 0.12, 0.16, 0.0],
        };

        // gather camera-relative instances from the world (collect first so the
        // Renderable query borrow ends before we look up each Transform)
        let renderables: Vec<(Entity, Shape, [f32; 3])> =
            self.app.world.query::<Renderable>().map(|(e, r)| (e, r.shape, r.color)).collect();
        let mut instances: Vec<(MeshId, InstanceRaw)> = Vec::with_capacity(renderables.len());
        for (e, shape, color) in renderables {
            // skip (don't panic) if a shape has no registered mesh yet
            let Some(&mesh) = self.mesh_ids.get(shape.index()) else { continue };
            if let Some(t) = self.app.world.get::<Transform>(e) {
                let model = t.render_matrix(cam.world_position);
                instances.push((mesh, instance_of(model, color)));
            }
        }

        // a quiet, dark indigo backdrop (kept below the objects' shadowed faces so
        // the lit solids clearly pop) with a barely-there breathing pulse
        let t = self.app.time.elapsed as f32;
        let pulse = |phase: f32, lo: f32, hi: f32| {
            let s = 0.5 + 0.5 * (t * 0.4 + phase).sin();
            (lo + (hi - lo) * s) as f64
        };
        let clear = [pulse(0.0, 0.005, 0.014), pulse(2.0, 0.004, 0.010), pulse(4.0, 0.014, 0.030), 1.0];

        match gpu.acquire() {
            Some(frame) => {
                raster.draw_scene(gpu, &frame, globals, &instances, clear);
                frame.present();
            }
            None => {
                // surface was lost/outdated and reconfigured; reconfigure to size
                let size = window.inner_size();
                gpu.resize(size.width, size.height);
            }
        }
    }
}
