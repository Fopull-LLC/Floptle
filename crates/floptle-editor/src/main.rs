//! # Floptle Editor
//!
//! The authoring application (binary `floptle`) — an egui shell over a live wgpu
//! viewport (ADR-0004). It renders the World **loaded from a `.ron` scene** with
//! the engine's PS1/retro look, and lets you select an object, move it, and save —
//! the first "open and interact with it" slice. Hierarchy/Inspector are stock egui
//! today; the dock shell, gizmos, import, and sculpt tools layer on next.

use std::sync::Arc;
use std::time::Instant;

use floptle_core::math::{DVec3, Mat4, Quat, Vec2, Vec3};
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

// ---- the overlay transform gizmo ----------------------------------------------
//
// A screen-space gizmo drawn over the selected object with egui's painter. The
// geometry (axis tips, rotation rings) is projected from the object's Transform
// once per frame into PHYSICAL pixels and cached in `GizmoFrame`, so window/device
// events can hit-test the cursor against it cheaply. Dragging a handle applies an
// absolute transform from a start-of-drag snapshot (no per-event accumulation, so
// no drift). The gizmo only PAINTS — it never registers an egui widget — so it
// never steals input from the panel or the RMB fly-camera; ownership is decided by
// our own pixel hit-test plus the existing `is_pointer_over_egui` gate.

/// Handle length on screen, in physical pixels (kept roughly constant with depth).
const GIZMO_PX: f32 = 90.0;
/// Cursor-to-handle pick radius, physical pixels.
const HANDLE_PX: f32 = 12.0;
/// Axis-scale drag sensitivity (scale factor per pixel along the axis).
const SCALE_SENS: f32 = 0.01;

/// The active editing tool. Bound to number keys 1-4 (5-9 reserved).
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tool {
    #[default]
    Select,
    Move,
    Rotate,
    Scale,
}

impl Tool {
    fn from_digit(n: u32) -> Option<Tool> {
        match n {
            1 => Some(Tool::Select),
            2 => Some(Tool::Move),
            3 => Some(Tool::Rotate),
            4 => Some(Tool::Scale),
            _ => None, // 5-9 reserved for future tools
        }
    }

    fn label(self) -> &'static str {
        match self {
            Tool::Select => "select",
            Tool::Move => "move",
            Tool::Rotate => "rotate",
            Tool::Scale => "scale",
        }
    }
}

/// Which part of the gizmo the cursor is over / grabbed. An axis handle's meaning
/// depends on the active `Tool` (move along / rotate about / scale along it).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Handle {
    AxisX,
    AxisY,
    AxisZ,
    Center,
}

impl Handle {
    /// Index into the world basis (X=0, Y=1, Z=2), or `None` for the center.
    fn axis_index(self) -> Option<usize> {
        match self {
            Handle::AxisX => Some(0),
            Handle::AxisY => Some(1),
            Handle::AxisZ => Some(2),
            Handle::Center => None,
        }
    }
}

/// Cached, projected gizmo geometry for the current frame (all in physical pixels).
struct GizmoFrame {
    center: Vec2,
    /// Axis arrow tips; `None` for an axis that projects behind the camera.
    tips: [Option<Vec2>; 3],
    /// Rotation-ring polylines (only filled for the Rotate tool).
    ring_pts: [Vec<Vec2>; 3],
    /// Which handle the cursor is hovering this frame, if any.
    hovered: Option<Handle>,
}

/// A start-of-drag snapshot, so drags apply an absolute transform (no drift).
#[derive(Clone, Copy)]
struct DragState {
    handle: Handle,
    /// The entity this snapshot belongs to — guards against the selection
    /// changing mid-drag and applying the wrong object's start transform.
    entity: Entity,
    start_xf: Transform,
    cursor_start: Vec2,
}

/// World basis vector for axis `i` (X=0, Y=1, Z=2).
fn axis_world(i: usize) -> Vec3 {
    [Vec3::X, Vec3::Y, Vec3::Z][i]
}

fn handle_for_axis(i: usize) -> Handle {
    [Handle::AxisX, Handle::AxisY, Handle::AxisZ][i]
}

/// Map a top-row number key to its digit (1-9), else `None`.
fn digit_of(code: KeyCode) -> Option<u32> {
    match code {
        KeyCode::Digit1 => Some(1),
        KeyCode::Digit2 => Some(2),
        KeyCode::Digit3 => Some(3),
        KeyCode::Digit4 => Some(4),
        KeyCode::Digit5 => Some(5),
        KeyCode::Digit6 => Some(6),
        KeyCode::Digit7 => Some(7),
        KeyCode::Digit8 => Some(8),
        KeyCode::Digit9 => Some(9),
        _ => None,
    }
}

/// Project an absolute world point to physical-pixel screen space (camera-relative,
/// ADR-0015). Returns `None` when the point is behind the camera.
fn project(world: DVec3, cam_world: DVec3, vp: Mat4, w: f32, h: f32) -> Option<Vec2> {
    let rel = (world - cam_world).as_vec3();
    let clip = vp * rel.extend(1.0);
    if clip.w <= 1e-4 {
        return None;
    }
    let ndc = clip.truncate() / clip.w;
    Some(Vec2::new((ndc.x * 0.5 + 0.5) * w, (1.0 - (ndc.y * 0.5 + 0.5)) * h))
}

/// Distance from point `p` to segment `a`–`b` (pixel space).
fn seg_dist(p: Vec2, a: Vec2, b: Vec2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_squared();
    let t = if len2 < 1e-6 { 0.0 } else { ((p - a).dot(ab) / len2).clamp(0.0, 1.0) };
    (p - (a + ab * t)).length()
}

/// Build the gizmo geometry for the selected entity and hit-test the cursor.
fn build_gizmo(
    tool: Tool,
    selection: Option<Entity>,
    world: &World,
    cursor: Option<Vec2>,
    cam_world: DVec3,
    vp: Mat4,
    w: f32,
    h: f32,
) -> Option<GizmoFrame> {
    if tool == Tool::Select {
        return None;
    }
    let e = selection?;
    let t = world.get::<Transform>(e)?;
    let center = project(t.translation, cam_world, vp, w, h)?;

    // Pixel-constant handle length: world units that subtend ~GIZMO_PX at this depth
    // (60° vertical fov). Clamp the near distance so a close object doesn't explode.
    let dist = (t.translation - cam_world).length().max(0.4) as f32;
    let axis_len = GIZMO_PX * 2.0 * dist * (30f32.to_radians()).tan() / h;

    let mut tips = [None; 3];
    for i in 0..3 {
        let tip_world = t.translation + (axis_world(i) * axis_len).as_dvec3();
        tips[i] = project(tip_world, cam_world, vp, w, h);
    }

    let mut ring_pts: [Vec<Vec2>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    if tool == Tool::Rotate {
        const N: usize = 48;
        for i in 0..3 {
            let u = axis_world((i + 1) % 3);
            let v = axis_world((i + 2) % 3);
            let mut pts = Vec::with_capacity(N + 1);
            for k in 0..=N {
                let a = (k as f32) / (N as f32) * std::f32::consts::TAU;
                let p = t.translation + ((u * a.cos() + v * a.sin()) * axis_len).as_dvec3();
                if let Some(s) = project(p, cam_world, vp, w, h) {
                    pts.push(s);
                }
            }
            ring_pts[i] = pts;
        }
    }

    let hovered = cursor.and_then(|c| hit_test(tool, c, center, &tips, &ring_pts));
    Some(GizmoFrame { center, tips, ring_pts, hovered })
}

/// Nearest gizmo handle to the cursor within `HANDLE_PX`, if any.
fn hit_test(
    tool: Tool,
    cursor: Vec2,
    center: Vec2,
    tips: &[Option<Vec2>; 3],
    rings: &[Vec<Vec2>; 3],
) -> Option<Handle> {
    let mut cands: Vec<(Handle, f32)> = Vec::new();
    match tool {
        Tool::Move | Tool::Scale => {
            for i in 0..3 {
                if let Some(tip) = tips[i] {
                    cands.push((handle_for_axis(i), seg_dist(cursor, center, tip)));
                }
            }
            cands.push((Handle::Center, (cursor - center).length()));
        }
        Tool::Rotate => {
            for i in 0..3 {
                let mut dmin = f32::INFINITY;
                for win in rings[i].windows(2) {
                    dmin = dmin.min(seg_dist(cursor, win[0], win[1]));
                }
                cands.push((handle_for_axis(i), dmin));
            }
        }
        Tool::Select => {}
    }
    cands
        .into_iter()
        .filter(|(_, d)| *d <= HANDLE_PX)
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(h, _)| h)
}

/// Brighten a handle color toward white when it is hovered or grabbed.
fn brighten(c: egui::Color32, on: bool) -> egui::Color32 {
    if !on {
        return c;
    }
    let mix = |x: u8| ((x as u16 + 255) / 2) as u8;
    egui::Color32::from_rgb(mix(c.r()), mix(c.g()), mix(c.b()))
}

/// A small filled arrowhead at `to`, pointing away from `from`.
fn arrow_head(painter: &egui::Painter, from: egui::Pos2, to: egui::Pos2, col: egui::Color32) {
    let dir = to - from;
    let len = dir.length();
    if len < 1.0 {
        return;
    }
    let d = dir / len;
    let n = egui::vec2(-d.y, d.x);
    let s = 8.0;
    let p2 = to - d * s + n * (s * 0.5);
    let p3 = to - d * s - n * (s * 0.5);
    painter.add(egui::Shape::convex_polygon(vec![to, p2, p3], col, egui::Stroke::NONE));
}

/// Paint the cached gizmo with the egui painter. Geometry is physical pixels; the
/// painter works in logical points, so divide by `ppp`.
fn paint_gizmo(painter: &egui::Painter, g: &GizmoFrame, tool: Tool, grabbed: Option<Handle>, ppp: f32) {
    use egui::{Color32, Pos2, Stroke};
    let pt = |v: Vec2| Pos2::new(v.x / ppp, v.y / ppp);
    let axis_col = [
        Color32::from_rgb(220, 70, 70),
        Color32::from_rgb(80, 200, 90),
        Color32::from_rgb(80, 130, 235),
    ];
    let active = |h: Handle| grabbed == Some(h) || g.hovered == Some(h);
    let center = pt(g.center);
    match tool {
        Tool::Move => {
            for i in 0..3 {
                if let Some(tip) = g.tips[i] {
                    let on = active(handle_for_axis(i));
                    let col = brighten(axis_col[i], on);
                    let tp = pt(tip);
                    painter.line_segment([center, tp], Stroke::new(if on { 4.0 } else { 2.5 }, col));
                    arrow_head(painter, center, tp, col);
                }
            }
            let on = active(Handle::Center);
            painter.rect_filled(
                egui::Rect::from_center_size(center, egui::vec2(9.0, 9.0)),
                0.0,
                brighten(Color32::from_gray(210), on),
            );
        }
        Tool::Scale => {
            for i in 0..3 {
                if let Some(tip) = g.tips[i] {
                    let on = active(handle_for_axis(i));
                    let col = brighten(axis_col[i], on);
                    let tp = pt(tip);
                    painter.line_segment([center, tp], Stroke::new(if on { 4.0 } else { 2.5 }, col));
                    painter.rect_filled(egui::Rect::from_center_size(tp, egui::vec2(8.0, 8.0)), 0.0, col);
                }
            }
            let on = active(Handle::Center);
            painter.rect_filled(
                egui::Rect::from_center_size(center, egui::vec2(10.0, 10.0)),
                0.0,
                brighten(Color32::from_gray(210), on),
            );
        }
        Tool::Rotate => {
            for i in 0..3 {
                let on = active(handle_for_axis(i));
                let col = brighten(axis_col[i], on);
                let pts: Vec<Pos2> = g.ring_pts[i].iter().map(|v| pt(*v)).collect();
                if pts.len() >= 2 {
                    painter.line(pts, Stroke::new(if on { 3.5 } else { 2.0 }, col));
                }
            }
            painter.circle_filled(center, 3.0, Color32::from_gray(200));
        }
        Tool::Select => {}
    }
}

fn main() {
    env_logger::init();
    println!("{} editor v{}", floptle_core::ENGINE_NAME, floptle_core::ENGINE_VERSION);
    let event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut editor = Editor::default();
    event_loop.run_app(&mut editor).expect("run editor");
}

// Field order is drop order: every GPU-resource holder (raster/raymarch/retro/egui)
// must drop BEFORE `gpu` (the device + surface), so `gpu` is intentionally last.
#[derive(Default)]
struct Editor {
    window: Option<Arc<Window>>,
    raster: Option<Raster>,
    raymarch: Option<Raymarch>,
    retro: Option<Retro>,
    egui: Option<Egui>,
    camera: FlyCamera,
    input: Input,
    world: World,
    /// Mesh handles indexed by `Shape as usize` (Cube=0, Sphere=1).
    mesh_ids: Vec<MeshId>,
    render: RenderConfigDoc,
    scene_name: String,
    selection: Option<Entity>,
    /// Active editing tool (keys 1-4); drives which gizmo handles are shown.
    tool: Tool,
    /// Cursor position in physical pixels (cached from `CursorMoved`).
    cursor: Option<Vec2>,
    /// Gizmo geometry + hover state, rebuilt every frame.
    gizmo: Option<GizmoFrame>,
    /// The gizmo handle currently being dragged, if any.
    grabbed: Option<Handle>,
    /// Start-of-drag snapshot for the grabbed handle.
    drag: Option<DragState>,
    /// Left mouse held in the viewport — drag-moves the selected object.
    left_down: bool,
    last: Option<Instant>,
    started: Option<Instant>,
    gpu: Option<Gpu>,
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
        let now = Instant::now();
        self.last = Some(now);
        self.started = Some(now);
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
            // Always cache the cursor (even over the panel) so hit-testing and the
            // over-UI gate stay correct; device_event only gives deltas.
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = Some(Vec2::new(position.x as f32, position.y as f32));
            }
            _ if consumed => {}
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                // Don't switch tools while typing into a numeric field.
                let typing = self.egui.as_ref().is_some_and(|e| e.ctx.egui_wants_keyboard_input());
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
                        // Number keys 1-9 pick the active tool (5-9 reserved → no-op).
                        _ if pressed && !typing && digit_of(code).is_some() => {
                            if let Some(t) = digit_of(code).and_then(Tool::from_digit) {
                                self.set_tool(t);
                            }
                        }
                        _ => {}
                    }
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                // not consumed → the click is in the viewport.
                let pressed = state == ElementState::Pressed;
                self.left_down = pressed;
                if pressed {
                    let over_ui = self.egui.as_ref().is_some_and(|e| e.ctx.is_pointer_over_egui());
                    let hovered = self.gizmo.as_ref().and_then(|g| g.hovered);
                    if let (false, Some(h), Some(e)) = (over_ui, hovered, self.selection) {
                        // Grab the hovered handle and snapshot the start transform.
                        if let Some(t) = self.world.get::<Transform>(e) {
                            self.grabbed = Some(h);
                            self.drag = Some(DragState {
                                handle: h,
                                entity: e,
                                start_xf: *t,
                                cursor_start: self.cursor.unwrap_or(Vec2::ZERO),
                            });
                        }
                    }
                } else {
                    self.grabbed = None;
                    self.drag = None;
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Right, .. } => {
                let looking = state == ElementState::Pressed;
                self.input.looking = looking;
                if looking {
                    // The cursor is hidden/confined during look and stops reporting;
                    // drop the cached position so the gizmo hover doesn't freeze on.
                    self.cursor = None;
                }
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
            // Priority: RMB-look > grabbed gizmo handle > free camera-plane drag.
            if self.input.looking {
                self.camera.look(delta.0 as f32, delta.1 as f32);
            } else if self.grabbed.is_some() {
                self.gizmo_drag();
            } else if self.left_down {
                // grab-and-drag the selected object (unless the cursor is over a UI widget)
                let over_ui = self.egui.as_ref().is_some_and(|e| e.ctx.is_pointer_over_egui());
                if !over_ui {
                    self.drag_selected(delta.0 as f32, delta.1 as f32);
                }
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
            self.retro.as_mut(),
            self.egui.as_mut(),
            self.window.as_ref(),
        ) else {
            return;
        };
        let window = window.clone();

        let now = Instant::now();
        let dt = self.last.map(|l| (now - l).as_secs_f32()).unwrap_or(0.0);
        self.last = Some(now);
        let elapsed = self.started.map(|s| (now - s).as_secs_f32()).unwrap_or(0.0);
        self.camera.update(&self.input, dt);

        // ---- gather the scene from the World ----
        let aspect = gpu.config.width as f32 / gpu.config.height.max(1) as f32;
        let cam = self.camera.render_camera();
        let view_proj = cam.view_proj(aspect);

        // Rebuild the overlay gizmo for the selected object (projects + hit-tests).
        self.gizmo = build_gizmo(
            self.tool,
            self.selection,
            &self.world,
            self.cursor,
            cam.world_position,
            view_proj,
            gpu.config.width as f32,
            gpu.config.height.max(1) as f32,
        );

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
                params: [elapsed, 0.0, 0.0, 0.0], // time → the blob morphs
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
        let old_retro_h = self.render.retro_height;
        let world = &mut self.world;
        let selection = &mut self.selection;
        let render = &mut self.render;
        let scene_name = self.scene_name.clone();
        let gizmo = self.gizmo.as_ref();
        let grabbed = self.grabbed;
        let tool = &mut self.tool;
        let mut want_save = false;
        let full_output = ctx.run_ui(raw_input, |ui| {
            egui::Panel::left("inspector").default_size(280.0).show(ui, |ui| {
                ui.heading("Floptle Editor");
                ui.label(format!("scene: {scene_name}"));
                ui.separator();

                ui.label("Tools");
                ui.horizontal(|ui| {
                    for (t, key) in [
                        (Tool::Select, "1"),
                        (Tool::Move, "2"),
                        (Tool::Rotate, "3"),
                        (Tool::Scale, "4"),
                    ] {
                        let txt = format!("{} {}", key, t.label());
                        if ui.selectable_label(*tool == t, txt).clicked() {
                            *tool = t;
                        }
                    }
                });
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

                ui.collapsing("Rendering (scene)", |ui| {
                    ui.checkbox(&mut render.retro, "retro pixelization");
                    ui.add(egui::Slider::new(&mut render.retro_height, 80u32..=1080).text("pixel rows"));
                    ui.checkbox(&mut render.matter, "SDF matter");
                });
                ui.collapsing("Lighting (scene)", |ui| {
                    ui.label("direction");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut render.light_dir[0]).speed(0.02).prefix("x "));
                        ui.add(egui::DragValue::new(&mut render.light_dir[1]).speed(0.02).prefix("y "));
                        ui.add(egui::DragValue::new(&mut render.light_dir[2]).speed(0.02).prefix("z "));
                    });
                    ui.horizontal(|ui| {
                        ui.label("light");
                        ui.color_edit_button_rgb(&mut render.light_color);
                        ui.label("ambient");
                        ui.color_edit_button_rgb(&mut render.ambient);
                    });
                });
                ui.separator();

                if ui.button("💾  Save scene").clicked() {
                    want_save = true;
                }
                ui.add_space(8.0);
                ui.small("1 select · 2 move · 3 rotate · 4 scale");
                ui.small("RMB-drag: look · WASD: move · Space/Ctrl: up/down");
            });

            // The gizmo paints over the viewport on a top layer (above the scene,
            // below tooltips), clipped to the area right of the panel so handles
            // never draw over the inspector. It only draws — interaction is handled
            // in the window/device events against the cached hit-test.
            if let Some(g) = gizmo {
                let painter = ui
                    .ctx()
                    .layer_painter(egui::LayerId::new(egui::Order::Middle, egui::Id::new("gizmo")))
                    .with_clip_rect(ui.available_rect_before_wrap());
                paint_gizmo(&painter, g, *tool, grabbed, ui.ctx().pixels_per_point());
            }
        });
        egui.state.handle_platform_output(&window, full_output.platform_output);
        if self.render.retro_height != old_retro_h {
            retro.resize(gpu, self.render.retro_height.max(80));
        }

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

    /// Move the selected entity in the camera plane by a screen-pixel delta — the
    /// "grab and drag in the viewport" gizmo (the object follows the cursor).
    fn drag_selected(&mut self, dx: f32, dy: f32) {
        let Some(e) = self.selection else { return };
        let cam_pos = self.camera.position;
        let rot = self.camera.rotation();
        let right = rot * Vec3::X;
        let up = rot * Vec3::Y;
        let h = self.gpu.as_ref().map(|g| g.config.height.max(1) as f32).unwrap_or(720.0);
        if let Some(t) = self.world.get_mut::<Transform>(e) {
            let dist = (t.translation - cam_pos).length().max(0.1) as f32;
            // world units per screen pixel at the object's depth (60° vertical fov)
            let wpp = 2.0 * dist * (30f32.to_radians()).tan() / h;
            let d = right * (dx * wpp) - up * (dy * wpp);
            t.translation += d.as_dvec3();
        }
    }

    /// Switch the active tool and cancel any in-progress gizmo drag.
    fn set_tool(&mut self, tool: Tool) {
        self.tool = tool;
        self.grabbed = None;
        self.drag = None;
    }

    /// Apply a gizmo drag for the grabbed handle, as an ABSOLUTE transform from the
    /// start-of-drag snapshot (no per-event accumulation → no drift).
    fn gizmo_drag(&mut self) {
        let (Some(drag), Some(cursor), Some(e)) = (self.drag, self.cursor, self.selection) else {
            return;
        };
        // The snapshot must belong to the still-selected entity (guards against the
        // selection changing mid-drag and applying the wrong object's transform).
        if drag.entity != e {
            self.grabbed = None;
            self.drag = None;
            return;
        }
        let handle = drag.handle;
        let (w, h) = self
            .gpu
            .as_ref()
            .map(|g| (g.config.width as f32, g.config.height.max(1) as f32))
            .unwrap_or((1280.0, 720.0));
        let cam = self.camera.render_camera();
        let vp = cam.view_proj(w / h);
        let cam_world = cam.world_position;
        let start = drag.start_xf;
        let cursor_delta = cursor - drag.cursor_start;

        match self.tool {
            Tool::Move => {
                if let Some(i) = handle.axis_index() {
                    let dir = axis_world(i);
                    // Project the axis (a 1-unit step) to screen; the move distance is
                    // the cursor delta projected onto that screen direction.
                    let (Some(s0), Some(s1)) = (
                        project(start.translation, cam_world, vp, w, h),
                        project(start.translation + dir.as_dvec3(), cam_world, vp, w, h),
                    ) else {
                        return;
                    };
                    let sdir = s1 - s0;
                    let len2 = sdir.length_squared();
                    if len2 < 1e-6 {
                        return; // axis points (almost) straight at the camera
                    }
                    let units = cursor_delta.dot(sdir) / len2;
                    if let Some(t) = self.world.get_mut::<Transform>(e) {
                        t.translation = start.translation + (dir * units).as_dvec3();
                    }
                } else {
                    // Center handle: free move in the camera plane.
                    let rot = cam.rotation;
                    let right = rot * Vec3::X;
                    let up = rot * Vec3::Y;
                    let dist = (start.translation - cam_world).length().max(0.1) as f32;
                    let wpp = 2.0 * dist * (30f32.to_radians()).tan() / h;
                    let mv = right * (cursor_delta.x * wpp) - up * (cursor_delta.y * wpp);
                    if let Some(t) = self.world.get_mut::<Transform>(e) {
                        t.translation = start.translation + mv.as_dvec3();
                    }
                }
            }
            Tool::Rotate => {
                let Some(i) = handle.axis_index() else { return };
                let dir = axis_world(i);
                let Some(center) = project(start.translation, cam_world, vp, w, h) else {
                    return;
                };
                let v1 = drag.cursor_start - center;
                let v2 = cursor - center;
                if v1.length_squared() < 1.0 || v2.length_squared() < 1.0 {
                    return;
                }
                let mut angle = (v1.x * v2.y - v1.y * v2.x).atan2(v1.x * v2.x + v1.y * v2.y);
                // Screen-y points down; flip when the axis faces away from the camera
                // so a drag always spins the visible way.
                if dir.dot((start.translation - cam_world).as_vec3()) > 0.0 {
                    angle = -angle;
                }
                if let Some(t) = self.world.get_mut::<Transform>(e) {
                    t.rotation = (Quat::from_axis_angle(dir, angle) * start.rotation).normalize();
                }
            }
            Tool::Scale => {
                if let Some(i) = handle.axis_index() {
                    let dir = axis_world(i);
                    let (Some(s0), Some(s1)) = (
                        project(start.translation, cam_world, vp, w, h),
                        project(start.translation + dir.as_dvec3(), cam_world, vp, w, h),
                    ) else {
                        return;
                    };
                    let n = (s1 - s0).normalize_or_zero();
                    let factor = 1.0 + cursor_delta.dot(n) * SCALE_SENS;
                    if let Some(t) = self.world.get_mut::<Transform>(e) {
                        let mut sc = start.scale;
                        sc[i] = (start.scale[i] * factor).max(0.01);
                        t.scale = sc;
                    }
                } else {
                    // Center handle: uniform scale by the cursor's distance ratio.
                    let Some(center) = project(start.translation, cam_world, vp, w, h) else {
                        return;
                    };
                    let d0 = (drag.cursor_start - center).length().max(1.0);
                    let d1 = (cursor - center).length();
                    let factor = (d1 / d0).max(0.01);
                    if let Some(t) = self.world.get_mut::<Transform>(e) {
                        t.scale = (start.scale * factor).max(Vec3::splat(0.01));
                    }
                }
            }
            Tool::Select => {}
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
                matter: MatterDoc::Blob { scale: 1.0 },
            },
        ],
    }
}
