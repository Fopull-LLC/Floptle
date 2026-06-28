//! The windowed host: a winit event loop that owns the [`App`], opens a window,
//! creates the GPU, and drives `App::frame` on every redraw — the real Phase-1
//! core loop made visible.
//!
//! It now renders the Phase-1 demo: a **spinning textured quad** viewed through a
//! **free-fly camera** (RMB-drag to look, WASD to move, Space/Ctrl for up/down),
//! with **FPS in the title bar** (the day-one profiler the roadmap asks for). The
//! camera feeds `RenderCamera`; the quad's `Transform` feeds a camera-relative
//! model matrix — large-world-safe by construction (ADR-0015).

use std::sync::Arc;
use std::time::Instant;

use floptle_core::math::Mat4;
use floptle_core::transform::Transform;
use floptle_render::{Gpu, Raster};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

use crate::app::App;
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
            .with_title("Floptle — Phase 1   |   RMB-drag: look   ·   WASD: move")
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        let gpu = Gpu::new(window.clone());
        self.raster = Some(Raster::new(&gpu));
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
            self.raster.as_ref(),
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
        self.app.frame(dt); // spins the quad

        // FPS in the title bar — the day-one profiler (roadmap Phase 1)
        clock.fps_frames += 1;
        let since = now.duration_since(clock.fps_since).as_secs_f32();
        if since >= 0.5 {
            let fps = clock.fps_frames as f32 / since;
            window.set_title(&format!(
                "Floptle — Phase 1   |   {fps:.0} fps   |   RMB-drag: look   ·   WASD: move   ·   Space/Ctrl: up/down"
            ));
            clock.fps_frames = 0;
            clock.fps_since = now;
        }

        // camera view·projection + the quad's camera-relative model matrix
        let aspect = gpu.config.width as f32 / gpu.config.height.max(1) as f32;
        let cam = self.camera.render_camera();
        let view_proj = cam.view_proj(aspect);
        let model = self
            .app
            .world
            .query::<Transform>()
            .next()
            .map(|(_, t)| t.render_matrix(cam.world_position))
            .unwrap_or(Mat4::IDENTITY);

        // a slow indigo color-pulse behind the geometry, so the loop is visibly alive
        let t = self.app.time.elapsed as f32;
        let pulse = |phase: f32, lo: f32, hi: f32| {
            let s = 0.5 + 0.5 * (t * 0.4 + phase).sin();
            (lo + (hi - lo) * s) as f64
        };
        let color = [pulse(0.0, 0.02, 0.06), pulse(2.0, 0.01, 0.04), pulse(4.0, 0.08, 0.16), 1.0];

        match gpu.acquire() {
            Some(frame) => {
                raster.draw(gpu, &frame, view_proj, model, color);
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
