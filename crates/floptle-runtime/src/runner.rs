//! The windowed host: a winit event loop that owns the [`App`], opens a window,
//! creates the GPU, and drives `App::frame` on every redraw — the real core loop
//! made visible.
//!
//! It renders the Phase-2 scene: procedurally-generated **lit, depth-tested meshes**
//! (a spinning cube, a still sphere, a counter-spinning cube) viewed through a
//! **free-fly camera** (RMB-drag look, WASD move, Space/Ctrl up/down), with **FPS in
//! the title bar**. Each object uploads a camera-relative model matrix
//! (`Transform::render_matrix`, ADR-0015) into the instanced forward pass.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_core::Entity;
use floptle_render::{
    cube, instance_of, uv_sphere, FlyCamera, Globals, Gpu, Input, InstanceRaw, MeshId, Raster, TexId,
    Raymarch, RaymarchGlobals, Retro,
};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

use crate::app::{App, Renderable, Shape};

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
    /// Low-res offscreen target for the retro / PS1 look.
    retro: Option<Retro>,
    /// Whether the scene renders through the low-res retro target.
    retro_on: bool,
    /// Retro internal resolution (rows); width derives from the window aspect.
    retro_height: u32,
    /// Raymarched SDF-matter pass (the unified mesh+field thesis), its toggle, and
    /// the matter's world placement + scale.
    raymarch: Option<Raymarch>,
    raymarch_on: bool,
    matter_center: DVec3,
    matter_scale: f32,
    /// Placement of the baked mesh-volume matter (a converted cube, for now).
    volume_world: DVec3,
    volume_half: [f32; 3],
    volume_present: bool,
    /// Registered mesh handles, indexed by `Shape::index()`.
    mesh_ids: Vec<MeshId>,
    /// Imported glTF models drawn alongside the procedural primitives — every
    /// `.glb`/`.gltf` found under `assets/models/`.
    imported: Vec<Imported>,
    clock: Option<Clock>,
}

struct Clock {
    last: Instant,
    fps_since: Instant,
    fps_frames: u32,
}

/// An imported glTF model placed in the scene (managed by the runner until the
/// asset/scene system makes imported meshes first-class world entities).
struct Imported {
    mesh: MeshId,
    position: DVec3,
    /// Uniform scale that fits the (origin-centered) model to a target size.
    scale: f32,
    color: [f32; 3],
    /// Spin rate about Y (rad/s), so you see it lit from every side.
    spin: f32,
}

/// Find every `.glb`/`.gltf` under `dir` (recursive, sorted by path for a stable
/// layout). Drop any model into `assets/models/` and it gets imported on launch.
fn all_models(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| {
                x.eq_ignore_ascii_case("glb") || x.eq_ignore_ascii_case("gltf")
            }) {
                found.push(p);
            }
        }
    }
    found.sort();
    found
}

/// A vivid 16×16 magenta/cyan checker (RGBA8) — the cube's texture, so the bake's
/// nearest-surface color carry and the smin texture-fade are clearly visible.
fn vivid_checker() -> (Vec<u8>, u32, u32) {
    let n: usize = 16;
    let mut px = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let c = (x / 2 + y / 2) & 1;
            let col = if c == 0 { [230u8, 40, 200] } else { [40, 220, 230] };
            let i = (y * n + x) * 4;
            px[i] = col[0];
            px[i + 1] = col[1];
            px[i + 2] = col[2];
            px[i + 3] = 255;
        }
    }
    (px, n as u32, n as u32)
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
        // Registration order defines the Shape→MeshId mapping (Shape::index). The
        // procedural primitives have no texture (a white default shows their tint).
        let cube_id = raster.register(&gpu, &cube(0.7), None);
        let sphere_id = raster.register(&gpu, &uv_sphere(0.85, 24, 36), None);
        self.mesh_ids = vec![cube_id, sphere_id];

        // Import every model under assets/models/. A large model (a map/level) is
        // rendered FULL-SCALE as a walkable environment — floor dropped onto the
        // ground plane, the camera pulled back and its speed scaled so you can fly
        // through it. Small models become props you can fly up to. Each model's
        // per-material parts register separately so their textures bind correctly.
        let paths = all_models(Path::new("assets/models"));
        let mut env_size: Option<f32> = None;
        let mut prop_models: Vec<(Vec<(MeshId, [f32; 3])>, f32)> = Vec::new();
        for path in &paths {
            match floptle_assets::gltf_import::import(path) {
                Ok(model) => {
                    println!(
                        "  imported '{}' — {} parts, {} textures, size {:.2}",
                        model.name,
                        model.parts.len(),
                        model.textures.len(),
                        model.size
                    );
                    // Upload each material part with its base-color texture.
                    let part_ids: Vec<(MeshId, [f32; 3])> = model
                        .parts
                        .iter()
                        .map(|part| {
                            let tex = part.texture.map(|i| &model.textures[i]);
                            (raster.register(&gpu, &part.mesh, tex), part.base_color)
                        })
                        .collect();

                    if model.size > 20.0 && env_size.is_none() {
                        // environment: native scale, static, floor set to y = 0
                        let floor = -model.min[1] as f64;
                        for (mesh, color) in &part_ids {
                            self.imported.push(Imported {
                                mesh: *mesh,
                                position: DVec3::new(0.0, floor, 0.0),
                                scale: 1.0,
                                color: *color,
                                spin: 0.0,
                            });
                        }
                        env_size = Some(model.size);
                    } else {
                        prop_models.push((part_ids, model.size));
                    }
                }
                Err(e) => eprintln!("  could not import {}: {e}", path.display()),
            }
        }

        if let Some(s) = env_size {
            // Start with a 3/4 aerial overview of the whole level (looking down at
            // the floor center), with camera speed scaled so it's traversable —
            // fly down into it to explore.
            let s = s as f64;
            self.camera.position = DVec3::new(0.0, s * 0.5, s * 0.65);
            self.camera.pitch = -0.66; // ≈ looks at the floor center from here
            self.camera.speed = (s / 9.0).max(4.0);
            // Sit the procedural primitives on the floor near the level center as
            // small reference props.
            let ents: Vec<Entity> = self.app.world.query::<Renderable>().map(|(e, _)| e).collect();
            let m = ents.len();
            for (k, e) in ents.into_iter().enumerate() {
                if let Some(t) = self.app.world.get_mut::<Transform>(e) {
                    let x = (k as f64 - (m as f64 - 1.0) * 0.5) * 2.4;
                    t.translation = DVec3::new(x, 0.8, s * 0.30);
                }
            }
            // Small imported models (e.g. the Duck) as props on the floor too.
            let n = prop_models.len();
            for (i, (part_ids, size)) in prop_models.into_iter().enumerate() {
                let x = (i as f64 - (n as f64 - 1.0) * 0.5) * 3.0;
                let scale = 2.0 / size;
                for (mesh, color) in part_ids {
                    self.imported.push(Imported {
                        mesh,
                        position: DVec3::new(x, 1.3, s * 0.30 - 4.0),
                        scale,
                        color,
                        spin: 0.3,
                    });
                }
            }
        } else {
            // No environment: showcase every imported model in a spinning row.
            let n = prop_models.len();
            for (i, (part_ids, size)) in prop_models.into_iter().enumerate() {
                let x = (i as f64 - (n as f64 - 1.0) * 0.5) * 3.6;
                let scale = 2.6 / size;
                for (mesh, color) in part_ids {
                    self.imported.push(Imported {
                        mesh,
                        position: DVec3::new(x, 2.8, 0.0),
                        scale,
                        color,
                        spin: 0.3,
                    });
                }
            }
        }

        // Retro / PS1 look on by default: render the scene at a low internal
        // resolution and upscale nearest-neighbor. Toggle with P; [ / ] adjust it.
        self.retro_height = 240;
        self.retro_on = true;
        self.retro = Some(Retro::new(&gpu, self.retro_height));

        // SDF matter pass on by default (F toggles). First mesh→SDF conversion:
        // bake a textured cube into a distance + color volume and fuse it with the
        // analytic blob — the cube's checker texture crossfades into the blob's
        // iridescence across the smin seam.
        let mut raymarch = Raymarch::new(&gpu);
        let cube_data = cube(0.9);
        let positions: Vec<[f32; 3]> = cube_data.vertices.iter().map(|v| v.pos).collect();
        let uvs: Vec<[f32; 2]> = cube_data.vertices.iter().map(|v| v.uv).collect();
        let (tex_px, tw, th) = vivid_checker();
        let baked = floptle_field::bake(
            &positions,
            &cube_data.indices,
            &uvs,
            Some(floptle_field::TexRef { pixels: &tex_px, width: tw, height: th }),
            [1.0, 1.0, 1.0],
            48,
            3.0,
        );
        raymarch.set_volume(&gpu, &baked);
        self.volume_half = baked.half_extent;
        self.volume_present = true;
        self.raymarch_on = true;
        self.raymarch = Some(raymarch);

        // Place the fused matter (blob + baked cube) a few units in front of the
        // camera's start so it's visible immediately, at comparable sizes.
        let fwd = self.camera.rotation() * Vec3::NEG_Z;
        self.matter_center = self.camera.position + fwd.as_dvec3() * 6.0;
        self.matter_scale = 1.3;
        self.volume_world = self.matter_center + DVec3::new(1.6, 0.0, 0.0);

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
                    if let Some(retro) = self.retro.as_mut() {
                        retro.resize(gpu, self.retro_height);
                    }
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
                        KeyCode::KeyP if pressed => self.retro_on = !self.retro_on,
                        KeyCode::KeyF if pressed => self.raymarch_on = !self.raymarch_on,
                        KeyCode::BracketLeft if pressed => {
                            self.set_retro_height(self.retro_height.saturating_sub(40))
                        }
                        KeyCode::BracketRight if pressed => {
                            self.set_retro_height(self.retro_height + 40)
                        }
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
        let (Some(gpu), Some(raster), Some(retro), Some(raymarch), Some(window), Some(clock)) = (
            self.gpu.as_mut(),
            self.raster.as_mut(),
            self.retro.as_ref(),
            self.raymarch.as_ref(),
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
            let retro_mode = if self.retro_on {
                let (w, h) = retro.resolution();
                format!("retro {w}×{h}")
            } else {
                "retro off".to_string()
            };
            let matter = if self.raymarch_on { "matter on" } else { "matter off" };
            window.set_title(&format!(
                "Floptle   |   {fps:.0} fps   |   {retro_mode} · {matter}   |   P: retro · F: matter · [ ]: pixels · RMB+WASD"
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
        let mut instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> =
            Vec::with_capacity(renderables.len());
        for (e, shape, color) in renderables {
            // skip (don't panic) if a shape has no registered mesh yet
            let Some(&mesh) = self.mesh_ids.get(shape.index()) else { continue };
            if let Some(t) = self.app.world.get::<Transform>(e) {
                let model = t.render_matrix(cam.world_position);
                instances.push((mesh, None, instance_of(model, color)));
            }
        }
        // imported glTF models, fit-scaled and slowly spinning above the row
        for imp in &self.imported {
            let spun = Transform {
                translation: imp.position,
                rotation: Quat::from_rotation_y(self.app.time.elapsed as f32 * imp.spin),
                scale: Vec3::splat(imp.scale),
            };
            let model = spun.render_matrix(cam.world_position);
            instances.push((imp.mesh, None, instance_of(model, imp.color)));
        }

        // a quiet, dark indigo backdrop (kept below the objects' shadowed faces so
        // the lit solids clearly pop) with a barely-there breathing pulse
        let t = self.app.time.elapsed as f32;
        let pulse = |phase: f32, lo: f32, hi: f32| {
            let s = 0.5 + 0.5 * (t * 0.4 + phase).sin();
            (lo + (hi - lo) * s) as f64
        };
        let clear = [pulse(0.0, 0.005, 0.014), pulse(2.0, 0.004, 0.010), pulse(4.0, 0.014, 0.030), 1.0];

        // SDF-matter globals: matter center in camera-relative space, the shared
        // view_proj (for depth) + its inverse (to reconstruct rays that line up
        // with the meshes).
        let center_cam = (self.matter_center - cam.world_position).as_vec3();
        let vol_cam = (self.volume_world - cam.world_position).as_vec3();
        let present = if self.volume_present { 1.0 } else { 0.0 };
        let mut blobs = [[0.0f32; 4]; 16];
        blobs[0] = [center_cam.x, center_cam.y, center_cam.z, self.matter_scale];
        let rm_globals = RaymarchGlobals {
            view_proj: view_proj.to_cols_array_2d(),
            inv_view_proj: view_proj.inverse().to_cols_array_2d(),
            light_dir: [light.x, light.y, light.z, 0.0],
            light_color: [1.0, 0.98, 0.92, 0.0],
            ambient: [0.12, 0.12, 0.16, 0.0],
            bg: [clear[0] as f32, clear[1] as f32, clear[2] as f32, 1.0],
            center: [0.0; 4],
            params: [self.app.time.elapsed as f32, 1.0, 0.0, 0.0],
            vol_center: [vol_cam.x, vol_cam.y, vol_cam.z, present],
            vol_half: [self.volume_half[0], self.volume_half[1], self.volume_half[2], 0.7],
            blobs,
        };

        match gpu.acquire() {
            Some(frame) => {
                // targets: the low-res retro buffer, or the swapchain directly
                let (color, depth) = if self.retro_on {
                    (retro.color_view(), retro.depth_view())
                } else {
                    (&frame.view, gpu.depth_view())
                };

                // SDF matter first (clears + writes depth); the meshes then LOAD
                // those targets and share the one depth buffer, so matter and
                // Blender geometry occlude/intersect correctly.
                let raster_clear = if self.raymarch_on {
                    raymarch.draw_into(gpu, color, depth, rm_globals);
                    None
                } else {
                    Some(clear)
                };
                raster.draw_scene(gpu, color, depth, globals, &instances, raster_clear);

                if self.retro_on {
                    retro.blit(gpu, &frame);
                }
                frame.present();
            }
            None => {
                // surface was lost/outdated and reconfigured; reconfigure to size
                let size = window.inner_size();
                gpu.resize(size.width, size.height);
            }
        }
    }

    /// Set the retro internal resolution (clamped) and rebuild the target.
    fn set_retro_height(&mut self, h: u32) {
        self.retro_height = h.clamp(80, 1080);
        if let (Some(retro), Some(gpu)) = (self.retro.as_mut(), self.gpu.as_ref()) {
            retro.resize(gpu, self.retro_height);
        }
    }
}
