//! TEMPORARY: the smallest possible wgpu + winit loop, to tell an engine
//! problem from a machine problem. Clears a window and reports the presented
//! rate. If this reads 60 and the editor reads 20 on the same display, the
//! editor is doing something; if both read 20, the display path is.

use std::sync::Arc;
use std::time::Instant;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

struct App {
    window: Option<Arc<Window>>,
    gpu: Option<(wgpu::Device, wgpu::Queue, wgpu::Surface<'static>)>,
    t: Instant,
    n: u32,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        let w = Arc::new(el.create_window(Window::default_attributes()).unwrap());
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
        println!("formats {:?}\npresent modes {:?}", caps.formats, caps.present_modes);
        surface.configure(
            &device,
            &wgpu::SurfaceConfiguration {
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                format: caps.formats[0],
                width: size.width.max(1),
                height: size.height.max(1),
                present_mode: match std::env::var("PM").as_deref() {
                    Ok("mailbox") => wgpu::PresentMode::Mailbox,
                    Ok("immediate") => wgpu::PresentMode::Immediate,
                    _ => wgpu::PresentMode::Fifo,
                },
                desired_maximum_frame_latency: 2,
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
                let frame = match surface.get_current_texture() {
                    wgpu::CurrentSurfaceTexture::Success(t)
                    | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
                    _ => return,
                };
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
                self.n += 1;
                let el2 = self.t.elapsed().as_secs_f32();
                if el2 >= 2.0 {
                    println!("{:.1} fps ({:.1} ms)", self.n as f32 / el2, el2 * 1000.0 / self.n as f32);
                    self.t = Instant::now();
                    self.n = 0;
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
    el.run_app(&mut App { window: None, gpu: None, t: Instant::now(), n: 0 }).unwrap();
}
