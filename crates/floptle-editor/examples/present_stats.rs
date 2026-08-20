//! Frame-pacing probe with a DISTRIBUTION, not a mean. A mean fps cannot tell
//! "steady 60" from "alternating 2 ms and 30 ms", which is the entire question
//! when a high fps number feels choppy.
//!
//! PM=fifo|mailbox|immediate (default fifo). Also prints what winit reports for
//! the monitor refresh rate, because `Editor::smooth_dt` trusts that number.

use std::sync::Arc;
use std::time::Instant;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

struct App {
    window: Option<Arc<Window>>,
    gpu: Option<(wgpu::Device, wgpu::Queue, wgpu::Surface<'static>)>,
    last: Option<Instant>,
    samples: Vec<f32>,
    wait: Vec<f32>,
    t: Instant,
    reports: u32,
}

fn pct(sorted: &[f32], p: f32) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() - 1) as f32 * p).round() as usize;
    sorted[i]
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let mut attrs = Window::default_attributes();
        if std::env::var("FS").is_ok() {
            attrs = attrs.with_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
        }
        let w = Arc::new(el.create_window(attrs).unwrap());
        let inst = wgpu::Instance::default();
        let surface = inst.create_surface(w.clone()).unwrap();
        let adapter = pollster::block_on(inst.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .unwrap();
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).unwrap();
        let size = w.inner_size();
        let caps = surface.get_capabilities(&adapter);
        let mode = match std::env::var("PM").as_deref() {
            Ok("mailbox") => wgpu::PresentMode::Mailbox,
            Ok("immediate") => wgpu::PresentMode::Immediate,
            _ => wgpu::PresentMode::Fifo,
        };
        println!("adapter        {:?}", adapter.get_info().name);
        println!("backend        {:?}", adapter.get_info().backend);
        println!("present modes  {:?}", caps.present_modes);
        println!("requested      {:?}", mode);
        // The number `Editor::smooth_dt` snaps dt against. If this is None or
        // disagrees with the panel actually presenting, the snapping is either
        // inert or actively wrong.
        match w.current_monitor() {
            Some(m) => println!(
                "current_monitor {:?}  refresh {:?} mHz  size {:?}",
                m.name(),
                m.refresh_rate_millihertz(),
                m.size()
            ),
            None => println!("current_monitor NONE  <- smooth_dt sees refresh_period = 0 and does nothing"),
        }
        println!("---");
        surface.configure(
            &device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: caps.formats[0],
                width: size.width.max(1),
                height: size.height.max(1),
                present_mode: mode,
                desired_maximum_frame_latency: std::env::var("FL").ok().and_then(|v| v.parse().ok()).unwrap_or(2),
                alpha_mode: caps.alpha_modes[0],
                view_formats: vec![],
            },
        );
        self.window = Some(w);
        self.gpu = Some((device, queue, surface));
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _: WindowId, ev: WindowEvent) {
        let Some((device, queue, surface)) = self.gpu.as_ref() else { return };
        match ev {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::RedrawRequested => {
                let acq = Instant::now();
                let frame = match surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(t)
                    | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
                    _ => return,
                };
                self.wait.push(acq.elapsed().as_secs_f32() * 1000.0);
                let view = frame.texture.create_view(&Default::default());
                let mut enc = device.create_command_encoder(&Default::default());
                {
                    enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: None,
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view: &view,
                            depth_slice: None,
                            resolve_target: None,
                            ops: wgpu::Operations {
                                load: wgpu::LoadOp::Clear(wgpu::Color::BLUE),
                                store: wgpu::StoreOp::Store,
                            },
                        })],
                        depth_stencil_attachment: None,
                        timestamp_writes: None,
                        occlusion_query_set: None,
                        multiview_mask: None,
                    });
                }
                queue.submit([enc.finish()]);
                frame.present();

                let now = Instant::now();
                if let Some(l) = self.last {
                    self.samples.push((now - l).as_secs_f32() * 1000.0);
                }
                self.last = Some(now);

                if self.t.elapsed().as_secs_f32() >= 2.0 && self.samples.len() > 4 {
                    let mut s = self.samples.clone();
                    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    let mut w2 = self.wait.clone();
                    w2.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    let n = s.len();
                    let mean: f32 = s.iter().sum::<f32>() / n as f32;
                    // Throughput fps (1/mean frame time) vs the editor's
                    // EMA-of-1/dt, which is biased toward the fast frames.
                    let biased: f32 = self.samples.iter().map(|d| 1000.0 / d).sum::<f32>() / n as f32;
                    println!(
                        "n={:4}  true {:6.1} fps ({:5.2} ms mean)   EMA-style {:6.1} fps   \
                         p50 {:5.2}  p95 {:5.2}  p99 {:5.2}  min {:5.2}  max {:6.2}  \
                         | acquire p50 {:5.2} p99 {:5.2}",
                        n,
                        1000.0 / mean,
                        mean,
                        biased,
                        pct(&s, 0.50),
                        pct(&s, 0.95),
                        pct(&s, 0.99),
                        s[0],
                        s[n - 1],
                        pct(&w2, 0.50),
                        pct(&w2, 0.99),
                    );
                    self.samples.clear();
                    self.wait.clear();
                    self.t = Instant::now();
                    if let Some(w) = self.window.as_ref() {
                        println!(
                            "    current_monitor now: {:?}   available: {:?}",
                            w.current_monitor().map(|m| (m.name(), m.refresh_rate_millihertz())),
                            el.available_monitors()
                                .map(|m| (m.name(), m.refresh_rate_millihertz()))
                                .collect::<Vec<_>>()
                        );
                    }
                    self.reports += 1;
                    if self.reports >= 5 {
                        el.exit();
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _: &ActiveEventLoop) {
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }
}

fn main() {
    let el = EventLoop::new().unwrap();
    el.set_control_flow(ControlFlow::Poll);
    el.run_app(&mut App {
        window: None,
        gpu: None,
        last: None,
        samples: Vec::new(),
        wait: Vec::new(),
        t: Instant::now(),
        reports: 0,
    })
    .unwrap();
}
