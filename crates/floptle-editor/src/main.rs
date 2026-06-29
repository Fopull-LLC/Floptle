//! # Floptle Editor
//!
//! The authoring application (binary `floptle`) — an egui shell over a live wgpu
//! viewport (ADR-0004). It renders the World **loaded from a `.ron` scene** with
//! the engine's PS1/retro look, and lets you select an object, move it, and save —
//! the first "open and interact with it" slice. Hierarchy/Inspector are stock egui
//! today; the dock shell, gizmos, import, and sculpt tools layer on next.

use std::sync::Arc;
use std::time::Instant;

use floptle_core::math::{DVec3, Vec3};
use floptle_core::transform::Transform;
use floptle_core::{Entity, Matter, Name, World};
use floptle_render::{
    cube, instance_of, uv_sphere, FlyCamera, Globals, Gpu, Input, InstanceRaw, MeshId, Raster,
    Raymarch, RaymarchGlobals, Retro,
};
use floptle_scene::RenderConfigDoc;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

const SCENE_PATH: &str = "assets/scenes/first.ron";

fn main() {
    env_logger::init();
    println!("{} editor v{}", floptle_core::ENGINE_NAME, floptle_core::ENGINE_VERSION);
    let event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut editor = Editor::default();
    event_loop.run_app(&mut editor).expect("run editor");
}

#[derive(Default)]
struct Editor {
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    raster: Option<Raster>,
    raymarch: Option<Raymarch>,
    retro: Option<Retro>,
    camera: FlyCamera,
    input: Input,
    world: World,
    /// Mesh handles indexed by `Shape as usize` (Cube=0, Sphere=1).
    mesh_ids: Vec<MeshId>,
    render: RenderConfigDoc,
    scene_name: String,
    selection: Option<Entity>,
    last: Option<Instant>,
    egui: Option<Egui>,
}

struct Egui {
    ctx: egui::Context,
    state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
}

impl ApplicationHandler for Editor {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Floptle Editor")
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));
        let gpu = Gpu::new(window.clone());
        let mut raster = Raster::new(&gpu);
        let cube_id = raster.register(&gpu, &cube(0.7), None);
        let sphere_id = raster.register(&gpu, &uv_sphere(0.85, 24, 36), None);
        self.mesh_ids = vec![cube_id, sphere_id];
        self.raymarch = Some(Raymarch::new(&gpu));

        // Load the scene (or fall back to a tiny built-in default).
        let doc = floptle_scene::load(std::path::Path::new(SCENE_PATH)).unwrap_or_else(|e| {
            eprintln!("  no scene at {SCENE_PATH} ({e}); using built-in default");
            default_scene()
        });
        self.scene_name = doc.name.clone();
        self.render = doc.render;
        floptle_scene::spawn_into(&doc, &mut self.world);

        self.retro = Some(Retro::new(&gpu, self.render.retro_height.max(80)));

        let ctx = egui::Context::default();
        let state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        let renderer = egui_wgpu::Renderer::new(
            &gpu.device,
            gpu.surface_format(),
            egui_wgpu::RendererOptions {
                msaa_samples: 1,
                depth_stencil_format: None,
                dithering: false,
                predictable_texture_filtering: false,
            },
        );
        self.egui = Some(Egui { ctx, state, renderer });

        self.gpu = Some(gpu);
        self.raster = Some(raster);
        self.last = Some(Instant::now());
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // Feed egui first; if it consumed the event, the viewport ignores it.
        let consumed = if let (Some(egui), Some(window)) = (self.egui.as_mut(), self.window.as_ref())
        {
            egui.state.on_window_event(window, &event).consumed
        } else {
            false
        };

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.resize(size.width, size.height);
                    if let Some(retro) = self.retro.as_mut() {
                        retro.resize(gpu, self.render.retro_height.max(80));
                    }
                }
            }
            WindowEvent::RedrawRequested => self.render(),
            _ if consumed => {}
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

impl Editor {
    fn render(&mut self) {
        let (Some(gpu), Some(raster), Some(raymarch), Some(retro), Some(egui), Some(window)) = (
            self.gpu.as_mut(),
            self.raster.as_mut(),
            self.raymarch.as_ref(),
            self.retro.as_ref(),
            self.egui.as_mut(),
            self.window.as_ref(),
        ) else {
            return;
        };
        let window = window.clone();

        let now = Instant::now();
        let dt = self.last.map(|l| (now - l).as_secs_f32()).unwrap_or(0.0);
        self.last = Some(now);
        self.camera.update(&self.input, dt);

        // ---- gather the scene from the World ----
        let aspect = gpu.config.width as f32 / gpu.config.height.max(1) as f32;
        let cam = self.camera.render_camera();
        let view_proj = cam.view_proj(aspect);
        let light = Vec3::from(self.render.light_dir).normalize_or_zero();
        let globals = Globals {
            view_proj: view_proj.to_cols_array_2d(),
            light_dir: [light.x, light.y, light.z, 0.0],
            light_color: [
                self.render.light_color[0],
                self.render.light_color[1],
                self.render.light_color[2],
                0.0,
            ],
            ambient: [self.render.ambient[0], self.render.ambient[1], self.render.ambient[2], 0.0],
        };

        let ents: Vec<(Entity, Matter)> =
            self.world.query::<Matter>().map(|(e, m)| (e, m.clone())).collect();
        let mut instances: Vec<(MeshId, InstanceRaw)> = Vec::new();
        let mut blob: Option<(DVec3, f32)> = None;
        for (e, matter) in &ents {
            let Some(t) = self.world.get::<Transform>(*e) else { continue };
            match matter {
                Matter::Primitive { shape, color } => {
                    if let Some(&mesh) = self.mesh_ids.get(*shape as usize) {
                        let model = t.render_matrix(cam.world_position);
                        instances.push((mesh, instance_of(model, *color)));
                    }
                }
                Matter::Blob { scale } => {
                    blob = Some((t.translation, scale * t.scale.x));
                }
            }
        }

        let clear = [0.02f32, 0.02, 0.05, 1.0];
        let rm = blob.map(|(center, scale)| {
            let c = (center - cam.world_position).as_vec3();
            RaymarchGlobals {
                view_proj: view_proj.to_cols_array_2d(),
                inv_view_proj: view_proj.inverse().to_cols_array_2d(),
                light_dir: [light.x, light.y, light.z, 0.0],
                bg: [clear[0], clear[1], clear[2], 1.0],
                center: [c.x, c.y, c.z, scale.max(0.05)],
                params: [0.0; 4],
                vol_center: [0.0, 0.0, 0.0, 0.0], // no baked volume in v1
                vol_half: [1.0, 1.0, 1.0, 0.5],
            }
        });

        // ---- build the egui UI (mutating the World) ----
        let raw_input = egui.state.take_egui_input(&window);
        let ctx = egui.ctx.clone();
        let entity_names: Vec<(Entity, String)> = ents
            .iter()
            .map(|(e, _)| {
                (*e, self.world.get::<Name>(*e).map(|n| n.0.clone()).unwrap_or_else(|| "node".into()))
            })
            .collect();
        let world = &mut self.world;
        let selection = &mut self.selection;
        let scene_name = self.scene_name.clone();
        let mut want_save = false;
        let full_output = ctx.run_ui(raw_input, |ui| {
            egui::Panel::left("inspector").default_size(270.0).show(ui, |ui| {
                ui.heading("Floptle Editor");
                ui.label(format!("scene: {scene_name}"));
                ui.separator();
                ui.label("Hierarchy");
                for (e, name) in &entity_names {
                    if ui.selectable_label(*selection == Some(*e), name).clicked() {
                        *selection = Some(*e);
                    }
                }
                ui.separator();
                ui.label("Inspector");
                if let Some(e) = *selection {
                    if let Some(t) = world.get_mut::<Transform>(e) {
                        ui.label("translation");
                        ui.horizontal(|ui| {
                            ui.add(egui::DragValue::new(&mut t.translation.x).speed(0.05).prefix("x "));
                            ui.add(egui::DragValue::new(&mut t.translation.y).speed(0.05).prefix("y "));
                            ui.add(egui::DragValue::new(&mut t.translation.z).speed(0.05).prefix("z "));
                        });
                        let mut s = t.scale.x;
                        if ui.add(egui::DragValue::new(&mut s).speed(0.02).prefix("scale ")).changed() {
                            t.scale = Vec3::splat(s.max(0.01));
                        }
                    } else {
                        ui.label("(no transform)");
                    }
                } else {
                    ui.label("(nothing selected)");
                }
                ui.separator();
                if ui.button("💾  Save scene").clicked() {
                    want_save = true;
                }
                ui.add_space(8.0);
                ui.small("RMB-drag: look · WASD: move · Space/Ctrl: up/down");
            });
        });
        egui.state.handle_platform_output(&window, full_output.platform_output);

        // ---- draw: scene into the retro target, blit, then egui on top ----
        match gpu.acquire() {
            Some(frame) => {
                let (color, depth) = if self.render.retro {
                    (retro.color_view(), retro.depth_view())
                } else {
                    (&frame.view, gpu.depth_view())
                };
                let raster_clear = if let (Some(rm), true) = (rm, self.render.matter) {
                    raymarch.draw_into(gpu, color, depth, rm);
                    None
                } else {
                    Some(clear.map(|c| c as f64))
                };
                raster.draw_scene(gpu, color, depth, globals, &instances, raster_clear);
                if self.render.retro {
                    retro.blit(gpu, &frame);
                }

                // egui composited over the final frame
                let ppp = full_output.pixels_per_point;
                let tris = ctx.tessellate(full_output.shapes, ppp);
                let screen = egui_wgpu::ScreenDescriptor {
                    size_in_pixels: [gpu.config.width, gpu.config.height],
                    pixels_per_point: ppp,
                };
                for (id, delta) in &full_output.textures_delta.set {
                    egui.renderer.update_texture(&gpu.device, &gpu.queue, *id, delta);
                }
                let mut encoder = gpu
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("egui") });
                egui.renderer.update_buffers(&gpu.device, &gpu.queue, &mut encoder, &tris, &screen);
                {
                    let mut pass = encoder
                        .begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("egui"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &frame.view,
                                depth_slice: None,
                                resolve_target: None,
                                ops: wgpu::Operations {
                                    load: wgpu::LoadOp::Load,
                                    store: wgpu::StoreOp::Store,
                                },
                            })],
                            depth_stencil_attachment: None,
                            timestamp_writes: None,
                            occlusion_query_set: None,
                            multiview_mask: None,
                        })
                        .forget_lifetime();
                    egui.renderer.render(&mut pass, &tris, &screen);
                }
                gpu.queue.submit([encoder.finish()]);
                for id in &full_output.textures_delta.free {
                    egui.renderer.free_texture(id);
                }
                frame.present();
            }
            None => {
                let size = window.inner_size();
                gpu.resize(size.width, size.height);
            }
        }

        if want_save {
            self.save_scene();
        }
    }

    fn save_scene(&self) {
        let doc = floptle_scene::to_doc(self.scene_name.clone(), self.render, &self.world);
        match floptle_scene::save(&doc, std::path::Path::new(SCENE_PATH)) {
            Ok(()) => println!("  saved {SCENE_PATH}"),
            Err(e) => eprintln!("  save failed: {e}"),
        }
    }
}

/// A tiny built-in scene used if `assets/scenes/first.ron` is missing.
fn default_scene() -> floptle_scene::SceneDoc {
    use floptle_scene::*;
    SceneDoc {
        name: "first".into(),
        render: RenderConfigDoc::ps1(),
        nodes: vec![
            NodeDoc {
                name: "cube".into(),
                transform: TransformDoc { translation: [-2.0, 0.0, 0.0], ..Default::default() },
                matter: MatterDoc::Primitive { shape: ShapeDoc::Cube, color: [0.9, 0.45, 0.35] },
            },
            NodeDoc {
                name: "sphere".into(),
                transform: TransformDoc { translation: [2.0, 0.0, 0.0], ..Default::default() },
                matter: MatterDoc::Primitive { shape: ShapeDoc::Sphere, color: [0.4, 0.7, 0.95] },
            },
            NodeDoc {
                name: "blob".into(),
                transform: TransformDoc { translation: [0.0, 1.6, 0.0], ..Default::default() },
                matter: MatterDoc::Blob { scale: 1.3 },
            },
        ],
    }
}
