//! The windowed host: a winit event loop that owns the [`App`], opens a window,
//! creates the GPU, and drives `App::frame` on every redraw — the real Phase-1
//! core loop made visible.
//!
//! Rendering is currently a single **clear pass** (a slow color pulse, so you can
//! see the loop is alive) and **FPS in the title bar** (the day-one profiler the
//! roadmap asks for). Triangle → textured quad → free-fly camera are the next
//! fill-ins; the loop shape here doesn't change as they land.

use std::sync::Arc;
use std::time::Instant;

use floptle_render::Gpu;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

use crate::app::App;

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
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
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
            .with_title("Floptle — Phase 1")
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        self.gpu = Some(Gpu::new(window.clone()));
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
                if event.state == ElementState::Pressed
                    && event.physical_key == PhysicalKey::Code(KeyCode::Escape)
                {
                    event_loop.exit();
                }
            }
            WindowEvent::RedrawRequested => self.render(),
            _ => {}
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
        let (Some(gpu), Some(window), Some(clock)) =
            (self.gpu.as_mut(), self.window.as_ref(), self.clock.as_mut())
        else {
            return;
        };

        // advance the real core loop with the measured wall-clock delta
        let now = Instant::now();
        let dt = (now - clock.last).as_secs_f32();
        clock.last = now;
        self.app.frame(dt);

        // FPS in the title bar — the day-one profiler (roadmap Phase 1)
        clock.fps_frames += 1;
        let since = now.duration_since(clock.fps_since).as_secs_f32();
        if since >= 0.5 {
            let fps = clock.fps_frames as f32 / since;
            window.set_title(&format!("Floptle — Phase 1   |   {fps:.0} fps"));
            clock.fps_frames = 0;
            clock.fps_since = now;
        }

        // minimal render: a slow indigo color-pulse so the loop is visibly alive
        let t = self.app.time.elapsed as f32;
        let pulse = |phase: f32, lo: f32, hi: f32| {
            let s = 0.5 + 0.5 * (t * 0.4 + phase).sin();
            (lo + (hi - lo) * s) as f64
        };
        let color = [pulse(0.0, 0.02, 0.06), pulse(2.0, 0.01, 0.04), pulse(4.0, 0.08, 0.16), 1.0];

        match gpu.acquire() {
            Some(frame) => {
                gpu.clear(&frame, color);
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
