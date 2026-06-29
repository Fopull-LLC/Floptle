//! # Floptle Editor
//!
//! The authoring application (binary `floptle`) — an egui shell over a live wgpu
//! viewport (ADR-0004). It renders the World **loaded from a `.ron` scene** with
//! the engine's PS1/retro look, and lets you select an object, move it, and save —
//! the first "open and interact with it" slice. Hierarchy/Inspector are stock egui
//! today; the dock shell, gizmos, import, and sculpt tools layer on next.

use std::sync::Arc;
use std::time::Instant;

use floptle_core::math::{DVec3, EulerRot, Mat4, Quat, Vec2, Vec3, Vec4};
use floptle_core::transform::Transform;
use floptle_core::{Entity, Light, Matter, Name, Shape, World};
use floptle_render::{
    cube, instance_of, uv_sphere, FlyCamera, Globals, Gpu, Grid, Input, InstanceRaw, MeshId,
    Outline, Raster, Raymarch, RaymarchGlobals, Retro,
};
use floptle_scene::{MatterDoc, NodeDoc, ProjectConfigDoc, SceneDoc, ShapeDoc, TransformDoc};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

const SCENE_PATH: &str = "assets/scenes/first.ron";
const PROJECT_PATH: &str = "assets/project.ron";

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
/// Screen radius (px) of the Rotate tool's center trackball ring.
const CENTER_RING_PX: f32 = 52.0;
/// Trackball free-rotate sensitivity (radians per pixel).
const TRACKBALL_SENS: f32 = 0.01;

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
    /// Local-axis arrow tips; `None` for an axis that projects behind the camera.
    tips: [Option<Vec2>; 3],
    /// Rotation-ring polylines, one per local axis (only filled for the Rotate tool).
    ring_pts: [Vec<Vec2>; 3],
    /// A flat screen-space ring around the center: the free/trackball handle for
    /// Rotate, drawn so the center handle is grabbable (Move/Scale use a box).
    center_ring: Vec<Vec2>,
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

/// The object's LOCAL axis `i` expressed in world space (so the gizmo aligns with
/// the object's current orientation, not the world frame).
fn local_axis(rot: Quat, i: usize) -> Vec3 {
    rot * axis_world(i)
}

fn handle_for_axis(i: usize) -> Handle {
    [Handle::AxisX, Handle::AxisY, Handle::AxisZ][i]
}

/// Deferred editor commands raised by the UI inside `run_ui`, applied after the
/// frame (so they can call `&mut self` methods the UI closure can't reach).
#[derive(Default)]
struct EditorCmd {
    add: Option<MatterDoc>,
    delete: bool,
    duplicate: bool,
    copy: bool,
    paste: bool,
    undo: bool,
    redo: bool,
    /// An inspector widget changed this frame (opens a coalesced undo step).
    inspector_changed: bool,
    /// Dismiss the viewport context menu.
    close_menu: bool,
}

/// Editor reference-grid display + snapping settings.
#[derive(Clone, Copy)]
struct GridConfig {
    show: bool,
    /// Spacing between grid lines (world units) — also the snap increment.
    size: f32,
    /// Cells out from the center the grid extends.
    extent: i32,
    color: [f32; 3],
    alpha: f32,
    /// Snap moved/created objects to the grid.
    snap: bool,
}

impl Default for GridConfig {
    fn default() -> Self {
        Self { show: true, size: 1.0, extent: 24, color: [0.45, 0.45, 0.58], alpha: 0.32, snap: false }
    }
}

fn new_cube() -> MatterDoc {
    MatterDoc::Primitive { shape: ShapeDoc::Cube, color: [0.8, 0.5, 0.4] }
}
fn new_sphere() -> MatterDoc {
    MatterDoc::Primitive { shape: ShapeDoc::Sphere, color: [0.4, 0.6, 0.9] }
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

/// Snap each component of a world position to a grid `step` (no-op if step ≤ 0).
fn snap_dvec3(v: DVec3, step: f64) -> DVec3 {
    if step <= 1e-6 {
        return v;
    }
    DVec3::new((v.x / step).round() * step, (v.y / step).round() * step, (v.z / step).round() * step)
}

/// Nearest positive ray–sphere hit distance (`rd` must be unit), else `None`.
fn ray_sphere(ro: Vec3, rd: Vec3, center: Vec3, radius: f32) -> Option<f32> {
    let oc = ro - center;
    let b = oc.dot(rd);
    let c = oc.length_squared() - radius * radius;
    let disc = b * b - c;
    if disc < 0.0 {
        return None;
    }
    let s = disc.sqrt();
    let t0 = -b - s;
    if t0 > 1e-3 {
        return Some(t0);
    }
    let t1 = -b + s; // origin inside the sphere
    (t1 > 1e-3).then_some(t1)
}

/// A picking bounding-sphere radius for an entity's matter (world units, scaled).
fn bounding_radius(m: &Matter, t: &Transform) -> f32 {
    let s = t.scale.max_element();
    match m {
        // cube(0.7) half-diagonal ≈ 0.7·√3
        Matter::Primitive { shape: Shape::Cube, .. } => 0.7 * 1.732 * s,
        Matter::Primitive { shape: Shape::Sphere, .. } => 0.85 * s,
        Matter::Blob { scale } => 0.85 * scale * t.scale.x,
    }
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
    let rot = t.rotation;

    // Pixel-constant handle length: world units that subtend ~GIZMO_PX at this depth
    // (60° vertical fov). Clamp the near distance so a close object doesn't explode.
    let dist = (t.translation - cam_world).length().max(0.4) as f32;
    let axis_len = GIZMO_PX * 2.0 * dist * (30f32.to_radians()).tan() / h;

    // Tips follow the object's LOCAL axes, so the gizmo aligns with its orientation.
    let mut tips = [None; 3];
    for i in 0..3 {
        let tip_world = t.translation + (local_axis(rot, i) * axis_len).as_dvec3();
        tips[i] = project(tip_world, cam_world, vp, w, h);
    }

    // Rotation rings live in the planes spanned by the object's local axes.
    let mut ring_pts: [Vec<Vec2>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let mut center_ring: Vec<Vec2> = Vec::new();
    if tool == Tool::Rotate {
        const N: usize = 48;
        for i in 0..3 {
            let u = local_axis(rot, (i + 1) % 3);
            let v = local_axis(rot, (i + 2) % 3);
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
        // A flat screen-space trackball ring around the center — the free-rotate handle.
        const M: usize = 40;
        for k in 0..=M {
            let a = (k as f32) / (M as f32) * std::f32::consts::TAU;
            center_ring.push(center + Vec2::new(a.cos(), a.sin()) * CENTER_RING_PX);
        }
    }

    let hovered = cursor.and_then(|c| hit_test(tool, c, center, &tips, &ring_pts, &center_ring));
    Some(GizmoFrame { center, tips, ring_pts, center_ring, hovered })
}

/// Nearest gizmo handle to the cursor within `HANDLE_PX`, if any.
fn hit_test(
    tool: Tool,
    cursor: Vec2,
    center: Vec2,
    tips: &[Option<Vec2>; 3],
    rings: &[Vec<Vec2>; 3],
    center_ring: &[Vec2],
) -> Option<Handle> {
    let mut cands: Vec<(Handle, f32)> = Vec::new();
    let ring_dist = |ring: &[Vec2]| {
        let mut dmin = f32::INFINITY;
        for win in ring.windows(2) {
            dmin = dmin.min(seg_dist(cursor, win[0], win[1]));
        }
        dmin
    };
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
                cands.push((handle_for_axis(i), ring_dist(&rings[i])));
            }
            // The trackball ring (free rotate) — only when not closer to an axis ring.
            cands.push((Handle::Center, ring_dist(center_ring)));
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
            // The trackball (free-rotate) ring first, so axis rings draw on top.
            let on_c = active(Handle::Center);
            let cring: Vec<Pos2> = g.center_ring.iter().map(|v| pt(*v)).collect();
            if cring.len() >= 2 {
                painter.line(cring, Stroke::new(if on_c { 3.0 } else { 1.5 }, brighten(Color32::from_gray(170), on_c)));
            }
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
    /// Selection-outline post-process (silhouette mask + edge detect).
    outline: Option<Outline>,
    /// Editor reference-grid renderer.
    grid_render: Option<Grid>,
    egui: Option<Egui>,
    camera: FlyCamera,
    input: Input,
    world: World,
    /// Mesh handles indexed by `Shape as usize` (Cube=0, Sphere=1).
    mesh_ids: Vec<MeshId>,
    /// Project-wide render settings (retro / matter), edited in Project Settings.
    project: ProjectConfigDoc,
    /// Whether the Project Settings window is open.
    show_project_settings: bool,
    scene_name: String,
    /// Selected entities (multi-select); the gizmo/inspector act on the last one.
    selection: Vec<Entity>,
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
    /// Modifier key state (tracked from key events).
    ctrl: bool,
    shift: bool,
    /// Undo/redo history of whole-scene snapshots.
    history: History,
    /// Copied nodes (Ctrl+C), re-spawned by Ctrl+V.
    clipboard: Vec<floptle_scene::NodeDoc>,
    /// An inspector/gizmo edit session is open — coalesces a drag into one undo step.
    editing: bool,
    /// The pre-edit scene snapshot captured at the start of this frame.
    frame_snapshot: Option<floptle_scene::SceneDoc>,
    /// RMB press position + accumulated motion — distinguishes a look-drag from a
    /// context-menu click.
    rmb_press: Option<Vec2>,
    rmb_moved: f32,
    /// A pending viewport context menu at (screen-point, entity-under-cursor).
    context_menu: Option<(egui::Pos2, Option<Entity>)>,
    /// Reference grid + snap settings.
    grid: GridConfig,
    show_grid_settings: bool,
    last: Option<Instant>,
    started: Option<Instant>,
    gpu: Option<Gpu>,
}

/// Undo/redo stack of whole-scene snapshots (simple + robust for small scenes).
struct History {
    undo: Vec<floptle_scene::SceneDoc>,
    redo: Vec<floptle_scene::SceneDoc>,
    /// Max retained undo steps (a user preference later).
    max: usize,
}

impl Default for History {
    fn default() -> Self {
        Self { undo: Vec::new(), redo: Vec::new(), max: 32 }
    }
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
        floptle_scene::spawn_into(&doc, &mut self.world);

        // Project-wide render settings live in their own file, shared across scenes.
        self.project = floptle_scene::load_project(std::path::Path::new(PROJECT_PATH));

        self.retro = Some(Retro::new(&gpu, self.project.retro_height.max(80)));
        self.outline = Some(Outline::new(&gpu));
        self.grid_render = Some(Grid::new(&gpu));

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
                        retro.resize(gpu, self.project.retro_height.max(80));
                    }
                    if let Some(outline) = self.outline.as_mut() {
                        outline.resize(gpu, size.width, size.height);
                    }
                }
            }
            WindowEvent::RedrawRequested => self.render(),
            // Always cache the cursor (even over the panel) so hit-testing and the
            // over-UI gate stay correct; device_event only gives deltas.
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = Some(Vec2::new(position.x as f32, position.y as f32));
            }
            // Modifier state, tracked separately so Ctrl/Shift combos work even while
            // a field is focused (this event isn't gated by `consumed`).
            WindowEvent::ModifiersChanged(mods) => {
                self.ctrl = mods.state().control_key();
                self.shift = mods.state().shift_key();
                self.input.boost = self.shift;
            }
            _ if consumed => {}
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                // Don't trigger shortcuts/tools while typing into a field.
                let typing = self.egui.as_ref().is_some_and(|e| e.ctx.egui_wants_keyboard_input());
                if let PhysicalKey::Code(code) = event.physical_key {
                    // Held movement keys (suppressed while Ctrl is down so Ctrl+key
                    // combos don't also fly the camera). C moves DOWN (rebound from
                    // Ctrl, which is now the shortcut modifier).
                    match code {
                        KeyCode::KeyW if !self.ctrl => self.input.forward = pressed,
                        KeyCode::KeyS if !self.ctrl => self.input.back = pressed,
                        KeyCode::KeyA if !self.ctrl => self.input.left = pressed,
                        KeyCode::KeyD if !self.ctrl => self.input.right = pressed,
                        KeyCode::Space => self.input.up = pressed,
                        KeyCode::KeyC if !self.ctrl => self.input.down = pressed,
                        _ => {}
                    }
                    // Discrete commands fire on press only.
                    if pressed && !typing {
                        if self.ctrl {
                            match code {
                                KeyCode::KeyZ => self.undo(),
                                KeyCode::KeyY => self.redo(),
                                KeyCode::KeyC => self.copy_selected(),
                                KeyCode::KeyV => self.paste(),
                                KeyCode::KeyD => self.duplicate_selected(),
                                KeyCode::KeyA => self.select_all(),
                                KeyCode::KeyS => self.save_scene(),
                                _ => {}
                            }
                        } else {
                            match code {
                                KeyCode::Escape => event_loop.exit(),
                                KeyCode::Delete | KeyCode::Backspace => self.delete_selected(),
                                _ => {
                                    if let Some(t) = digit_of(code).and_then(Tool::from_digit) {
                                        self.set_tool(t);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                // not consumed → the click is in the viewport.
                let pressed = state == ElementState::Pressed;
                if pressed {
                    let over_ui = self.egui.as_ref().is_some_and(|e| e.ctx.is_pointer_over_egui());
                    let hovered = self.gizmo.as_ref().and_then(|g| g.hovered);
                    if !over_ui {
                        // Clicking the viewport dismisses an open context menu (but
                        // clicking the menu itself, which is over_ui, keeps it).
                        self.context_menu = None;
                        if let (Some(h), Some(e)) = (hovered, self.primary()) {
                            // On a gizmo handle → start an undoable edit and grab it.
                            if let Some(t) = self.world.get::<Transform>(e) {
                                let start_xf = *t;
                                self.begin_edit();
                                self.grabbed = Some(h);
                                self.drag = Some(DragState {
                                    handle: h,
                                    entity: e,
                                    start_xf,
                                    cursor_start: self.cursor.unwrap_or(Vec2::ZERO),
                                });
                            }
                        } else if let Some(cursor) = self.cursor {
                            // Empty viewport → pick: single-select, or Shift to add.
                            match self.pick(cursor) {
                                Some(e) if self.shift => self.select_toggle(e),
                                Some(e) => self.select_single(e),
                                None if !self.shift => self.selection.clear(),
                                None => {}
                            }
                        }
                    }
                } else {
                    self.grabbed = None;
                    self.drag = None;
                    self.editing = false;
                }
            }
            WindowEvent::MouseInput { state, button: MouseButton::Right, .. } => {
                let pressed = state == ElementState::Pressed;
                let over_ui = self.egui.as_ref().is_some_and(|e| e.ctx.is_pointer_over_egui());
                if pressed {
                    // Begin a possible look; if the cursor barely moves before release
                    // it's a click → open a context menu instead.
                    self.rmb_press = self.cursor;
                    self.rmb_moved = 0.0;
                    self.context_menu = None;
                    if !over_ui {
                        self.input.looking = true;
                        if let Some(window) = self.window.as_ref() {
                            let _ = window
                                .set_cursor_grab(CursorGrabMode::Confined)
                                .or_else(|_| window.set_cursor_grab(CursorGrabMode::Locked));
                            window.set_cursor_visible(false);
                        }
                        self.cursor = None;
                    }
                } else {
                    let was_looking = self.input.looking;
                    self.input.looking = false;
                    if let Some(window) = self.window.as_ref() {
                        let _ = window.set_cursor_grab(CursorGrabMode::None);
                        window.set_cursor_visible(true);
                    }
                    // A click (negligible motion) over the viewport → context menu.
                    if was_looking && self.rmb_moved < 6.0 {
                        if let Some(p) = self.rmb_press {
                            self.cursor = Some(p);
                            let ppp = self
                                .egui
                                .as_ref()
                                .map(|e| e.ctx.pixels_per_point())
                                .unwrap_or(1.0);
                            let hit = self.pick(p);
                            if let Some(e) = hit {
                                if self.shift {
                                    self.select_toggle(e);
                                } else if !self.selection.contains(&e) {
                                    self.select_single(e);
                                }
                            }
                            self.context_menu =
                                Some((egui::Pos2::new(p.x / ppp, p.y / ppp), hit));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event {
            // Priority: RMB-look > grabbed gizmo handle. (Free dragging an object now
            // requires the Move tool's center handle — no more accidental moves.)
            if self.input.looking {
                self.camera.look(delta.0 as f32, delta.1 as f32);
                self.rmb_moved += (delta.0.abs() + delta.1.abs()) as f32;
            } else if self.grabbed.is_some() {
                self.gizmo_drag();
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
        let (
            Some(gpu),
            Some(raster),
            Some(raymarch),
            Some(retro),
            Some(outline),
            Some(grid_render),
            Some(egui),
            Some(window),
        ) = (
            self.gpu.as_mut(),
            self.raster.as_mut(),
            self.raymarch.as_ref(),
            self.retro.as_mut(),
            self.outline.as_ref(),
            self.grid_render.as_mut(),
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

        // Capture this frame's pre-edit scene, so an inspector/gizmo edit can push it
        // as a single undo step (see `begin_edit`). Inlined (not via `self.snapshot()`)
        // so it only touches disjoint fields while gpu/egui are borrowed.
        self.frame_snapshot = Some(floptle_scene::to_doc(self.scene_name.clone(), &self.world));

        // ---- gather the scene from the World ----
        let aspect = gpu.config.width as f32 / gpu.config.height.max(1) as f32;
        let cam = self.camera.render_camera();
        let view_proj = cam.view_proj(aspect);

        // Rebuild the overlay gizmo for the selected object (projects + hit-tests).
        self.gizmo = build_gizmo(
            self.tool,
            self.selection.last().copied(),
            &self.world,
            self.cursor,
            cam.world_position,
            view_proj,
            gpu.config.width as f32,
            gpu.config.height.max(1) as f32,
        );

        // Lighting comes from the scene's mandatory Lighting node (a Light component).
        let light_node = self.world.query::<Light>().next().map(|(_, l)| *l).unwrap_or_default();
        let light = Vec3::from(light_node.direction).normalize_or_zero();
        let globals = Globals {
            view_proj: view_proj.to_cols_array_2d(),
            light_dir: [light.x, light.y, light.z, 0.0],
            light_color: [light_node.color[0], light_node.color[1], light_node.color[2], 0.0],
            ambient: [light_node.ambient[0], light_node.ambient[1], light_node.ambient[2], 0.0],
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

        // Selection outline source: render the selected object's silhouette into the
        // outline mask (a mesh instance, or the blob via raymarch), then a post-pass
        // edge-detects it. `mask_mesh` non-empty → mesh; `mask_blob` → the SDF blob.
        let mut mask_mesh: Vec<(MeshId, InstanceRaw)> = Vec::new();
        let mut mask_blob = false;
        if let Some(e) = self.selection.last().copied() {
            if let (Some(m), Some(t)) =
                (self.world.get::<Matter>(e), self.world.get::<Transform>(e))
            {
                match m {
                    Matter::Primitive { shape, .. } => {
                        if let Some(&mesh) = self.mesh_ids.get(*shape as usize) {
                            let model = t.render_matrix(cam.world_position);
                            mask_mesh.push((mesh, instance_of(model, [1.0, 1.0, 1.0])));
                        }
                    }
                    Matter::Blob { .. } => mask_blob = true,
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
                light_color: [light_node.color[0], light_node.color[1], light_node.color[2], 0.0],
                ambient: [light_node.ambient[0], light_node.ambient[1], light_node.ambient[2], 0.0],
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
        // Every named entity, Matter nodes and the Lighting node alike.
        let entity_names: Vec<(Entity, String)> =
            self.world.query::<Name>().map(|(e, n)| (e, n.0.clone())).collect();
        let old_retro_h = self.project.retro_height;
        let world = &mut self.world;
        let selection = &mut self.selection;
        let project = &mut self.project;
        let show_project_settings = &mut self.show_project_settings;
        let grid = &mut self.grid;
        let show_grid_settings = &mut self.show_grid_settings;
        let scene_name = self.scene_name.clone();
        let gizmo = self.gizmo.as_ref();
        let grabbed = self.grabbed;
        let tool = &mut self.tool;
        let context_menu = self.context_menu;
        let mut cmd = EditorCmd::default();
        let mut want_save = false;
        let mut want_save_project = false;
        let full_output = ctx.run_ui(raw_input, |ui| {
            let primary = selection.last().copied();
            // ---- top menu bar ----
            egui::Panel::top("menu_bar").show(ui, |ui| {
                egui::MenuBar::new().ui(ui, |ui| {
                    ui.menu_button("File", |ui| {
                        if ui.button("Save Scene").clicked() {
                            want_save = true;
                            ui.close();
                        }
                        if ui.button("Save Project").clicked() {
                            want_save_project = true;
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("Exit").clicked() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    });
                    ui.menu_button("Edit", |ui| {
                        if ui.button("Undo  (Ctrl+Z)").clicked() { cmd.undo = true; ui.close(); }
                        if ui.button("Redo  (Ctrl+Y)").clicked() { cmd.redo = true; ui.close(); }
                        ui.separator();
                        if ui.button("Copy  (Ctrl+C)").clicked() { cmd.copy = true; ui.close(); }
                        if ui.button("Paste  (Ctrl+V)").clicked() { cmd.paste = true; ui.close(); }
                        if ui.button("Duplicate  (Ctrl+D)").clicked() { cmd.duplicate = true; ui.close(); }
                        if ui.button("Delete  (Del)").clicked() { cmd.delete = true; ui.close(); }
                        ui.separator();
                        if ui.button("Project Settings…").clicked() {
                            *show_project_settings = true;
                            ui.close();
                        }
                    });
                    ui.menu_button("Add", |ui| {
                        if ui.button("Cube").clicked() { cmd.add = Some(new_cube()); ui.close(); }
                        if ui.button("Sphere").clicked() { cmd.add = Some(new_sphere()); ui.close(); }
                        if ui.button("Blob").clicked() {
                            cmd.add = Some(MatterDoc::Blob { scale: 1.0 });
                            ui.close();
                        }
                    });
                    ui.menu_button("View", |ui| {
                        ui.checkbox(&mut grid.show, "Grid");
                        ui.checkbox(&mut grid.snap, "Snap to grid");
                        if ui.button("Grid Settings…").clicked() {
                            *show_grid_settings = true;
                            ui.close();
                        }
                    });
                    ui.menu_button("Project", |ui| {
                        if ui.button("Settings…").clicked() {
                            *show_project_settings = true;
                            ui.close();
                        }
                    });
                });
            });

            // ---- left inspector panel ----
            egui::Panel::left("inspector").default_size(290.0).show(ui, |ui| {
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

                ui.horizontal(|ui| {
                    ui.label("Hierarchy");
                    if ui.small_button("+ Cube").clicked() { cmd.add = Some(new_cube()); }
                    if ui.small_button("+ Sphere").clicked() { cmd.add = Some(new_sphere()); }
                    if ui.small_button("+ Blob").clicked() { cmd.add = Some(MatterDoc::Blob { scale: 1.0 }); }
                });
                for (e, name) in &entity_names {
                    let resp = ui.selectable_label(selection.contains(e), name);
                    if resp.clicked() {
                        if ui.input(|i| i.modifiers.shift) {
                            if let Some(pos) = selection.iter().position(|x| x == e) {
                                selection.remove(pos);
                            } else {
                                selection.push(*e);
                            }
                        } else {
                            selection.clear();
                            selection.push(*e);
                        }
                    }
                    if resp.secondary_clicked() && !selection.contains(e) {
                        selection.clear();
                        selection.push(*e);
                    }
                    resp.context_menu(|ui| {
                        if ui.button("Duplicate").clicked() { cmd.duplicate = true; ui.close(); }
                        if ui.button("Copy").clicked() { cmd.copy = true; ui.close(); }
                        if ui.button("Delete").clicked() { cmd.delete = true; ui.close(); }
                    });
                }
                ui.separator();

                ui.label("Inspector");
                if selection.len() > 1 {
                    ui.small(format!("{} selected", selection.len()));
                }
                match primary {
                    Some(e) if world.get::<Light>(e).is_some() => {
                        if let Some(l) = world.get_mut::<Light>(e) {
                            ui.label("Lighting node");
                            ui.label("direction");
                            ui.horizontal(|ui| {
                                cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut l.direction[0]).speed(0.02).prefix("x ")).changed();
                                cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut l.direction[1]).speed(0.02).prefix("y ")).changed();
                                cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut l.direction[2]).speed(0.02).prefix("z ")).changed();
                            });
                            ui.horizontal(|ui| {
                                ui.label("light");
                                cmd.inspector_changed |= ui.color_edit_button_rgb(&mut l.color).changed();
                                ui.label("ambient");
                                cmd.inspector_changed |= ui.color_edit_button_rgb(&mut l.ambient).changed();
                            });
                        }
                    }
                    Some(e) if world.get::<Transform>(e).is_some() => {
                        // name
                        if let Some(n) = world.get_mut::<Name>(e) {
                            ui.horizontal(|ui| {
                                ui.label("name");
                                cmd.inspector_changed |= ui.text_edit_singleline(&mut n.0).changed();
                            });
                        }
                        // matter-specific (shape + color, or blob scale)
                        if let Some(m) = world.get_mut::<Matter>(e) {
                            match m {
                                Matter::Primitive { shape, color } => {
                                    ui.horizontal(|ui| {
                                        ui.label("shape");
                                        egui::ComboBox::from_id_salt("shape")
                                            .selected_text(format!("{shape:?}"))
                                            .show_ui(ui, |ui| {
                                                cmd.inspector_changed |= ui.selectable_value(shape, Shape::Cube, "Cube").clicked();
                                                cmd.inspector_changed |= ui.selectable_value(shape, Shape::Sphere, "Sphere").clicked();
                                            });
                                        ui.label("color");
                                        cmd.inspector_changed |= ui.color_edit_button_rgb(color).changed();
                                    });
                                }
                                Matter::Blob { scale } => {
                                    cmd.inspector_changed |= ui
                                        .add(egui::DragValue::new(scale).speed(0.02).prefix("blob size ").range(0.05..=50.0))
                                        .changed();
                                }
                            }
                        }
                        // transform
                        if let Some(t) = world.get_mut::<Transform>(e) {
                            ui.label("translation");
                            ui.horizontal(|ui| {
                                cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut t.translation.x).speed(0.05).prefix("x ")).changed();
                                cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut t.translation.y).speed(0.05).prefix("y ")).changed();
                                cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut t.translation.z).speed(0.05).prefix("z ")).changed();
                            });
                            ui.label("rotation (deg)");
                            let (ey, ex, ez) = t.rotation.to_euler(EulerRot::YXZ);
                            let mut deg = [ey.to_degrees(), ex.to_degrees(), ez.to_degrees()];
                            let mut rot_changed = false;
                            ui.horizontal(|ui| {
                                rot_changed |= ui.add(egui::DragValue::new(&mut deg[0]).speed(1.0).prefix("y ")).changed();
                                rot_changed |= ui.add(egui::DragValue::new(&mut deg[1]).speed(1.0).prefix("x ")).changed();
                                rot_changed |= ui.add(egui::DragValue::new(&mut deg[2]).speed(1.0).prefix("z ")).changed();
                            });
                            if rot_changed {
                                t.rotation = Quat::from_euler(
                                    EulerRot::YXZ,
                                    deg[0].to_radians(),
                                    deg[1].to_radians(),
                                    deg[2].to_radians(),
                                );
                                cmd.inspector_changed = true;
                            }
                            ui.label("scale");
                            ui.horizontal(|ui| {
                                cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut t.scale.x).speed(0.02).prefix("x ")).changed();
                                cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut t.scale.y).speed(0.02).prefix("y ")).changed();
                                cmd.inspector_changed |= ui.add(egui::DragValue::new(&mut t.scale.z).speed(0.02).prefix("z ")).changed();
                            });
                        }
                    }
                    Some(_) => {
                        ui.label("(no editable properties)");
                    }
                    None => {
                        ui.label("(nothing selected)");
                    }
                }
                ui.separator();

                if ui.button("💾  Save scene").clicked() {
                    want_save = true;
                }
                ui.add_space(8.0);
                ui.small("1 select · 2 move · 3 rotate · 4 scale");
                ui.small("LMB select · Shift+LMB multi · RMB-drag look · RMB-click menu");
                ui.small("WASD move · Space/C up/down · Ctrl+Z/Y/C/V/D · Del");
            });

            // ---- project settings window (project-wide rendering) ----
            egui::Window::new("Project Settings")
                .open(show_project_settings)
                .resizable(false)
                .default_width(280.0)
                .show(ui.ctx(), |ui| {
                    ui.label("Rendering — applies to every scene");
                    ui.separator();
                    if ui.checkbox(&mut project.retro, "retro pixelization").changed() {
                        want_save_project = true;
                    }
                    if ui
                        .add(egui::Slider::new(&mut project.retro_height, 80u32..=1080).text("pixel rows"))
                        .changed()
                    {
                        want_save_project = true;
                    }
                    if ui.checkbox(&mut project.matter, "SDF matter").changed() {
                        want_save_project = true;
                    }
                    ui.add_space(6.0);
                    ui.small("saved to assets/project.ron");
                });

            // ---- grid settings window ----
            egui::Window::new("Grid Settings")
                .open(show_grid_settings)
                .resizable(false)
                .default_width(240.0)
                .show(ui.ctx(), |ui| {
                    ui.checkbox(&mut grid.show, "show grid");
                    ui.checkbox(&mut grid.snap, "snap objects to grid");
                    ui.add(egui::Slider::new(&mut grid.size, 0.1..=10.0).text("cell size"));
                    ui.add(egui::Slider::new(&mut grid.extent, 4..=120).text("extent (cells)"));
                    ui.add(egui::Slider::new(&mut grid.alpha, 0.0..=1.0).text("opacity"));
                    ui.horizontal(|ui| {
                        ui.label("color");
                        ui.color_edit_button_rgb(&mut grid.color);
                    });
                });

            // ---- viewport context menu (RMB click on an object / empty space) ----
            if let Some((pos, hit)) = context_menu {
                egui::Area::new(egui::Id::new("ctx_menu"))
                    .order(egui::Order::Foreground)
                    .fixed_pos(pos)
                    .show(ui.ctx(), |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.set_max_width(150.0);
                            if hit.is_some() {
                                if ui.button("Duplicate").clicked() {
                                    cmd.duplicate = true;
                                    cmd.close_menu = true;
                                }
                                if ui.button("Copy").clicked() {
                                    cmd.copy = true;
                                    cmd.close_menu = true;
                                }
                                if ui.button("Delete").clicked() {
                                    cmd.delete = true;
                                    cmd.close_menu = true;
                                }
                                ui.separator();
                            }
                            if ui.button("Paste").clicked() {
                                cmd.paste = true;
                                cmd.close_menu = true;
                            }
                            ui.menu_button("Add", |ui| {
                                if ui.button("Cube").clicked() {
                                    cmd.add = Some(new_cube());
                                    cmd.close_menu = true;
                                    ui.close();
                                }
                                if ui.button("Sphere").clicked() {
                                    cmd.add = Some(new_sphere());
                                    cmd.close_menu = true;
                                    ui.close();
                                }
                                if ui.button("Blob").clicked() {
                                    cmd.add = Some(MatterDoc::Blob { scale: 1.0 });
                                    cmd.close_menu = true;
                                    ui.close();
                                }
                            });
                        });
                    });
            }

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
        if self.project.retro_height != old_retro_h {
            retro.resize(gpu, self.project.retro_height.max(80));
        }

        // ---- draw: scene into the retro target, blit, then egui on top ----
        match gpu.acquire() {
            Some(frame) => {
                let (color, depth) = if self.project.retro {
                    (retro.color_view(), retro.depth_view())
                } else {
                    (&frame.view, gpu.depth_view())
                };
                let raster_clear = if let (Some(rm), true) = (rm, self.project.matter) {
                    raymarch.draw_into(gpu, color, depth, rm);
                    None
                } else {
                    Some(clear.map(|c| c as f64))
                };
                raster.draw_scene(gpu, color, depth, globals, &instances, raster_clear);
                if self.grid.show {
                    let c = self.grid.color;
                    grid_render.draw(
                        gpu,
                        color,
                        depth,
                        view_proj,
                        cam.world_position,
                        self.grid.size,
                        self.grid.extent,
                        [c[0], c[1], c[2], self.grid.alpha],
                    );
                }
                if self.project.retro {
                    retro.blit(gpu, &frame);
                }

                // Selection outline: mask the selected object's silhouette (full
                // frame res, so it stays crisp over the retro scene) then edge-detect
                // it onto the frame. Works for meshes and the SDF blob alike.
                let masked = if !mask_mesh.is_empty() {
                    raster.draw_mask(gpu, outline.mask_view(), globals, &mask_mesh);
                    true
                } else if let (true, Some(rm)) = (mask_blob, rm) {
                    raymarch.draw_mask(gpu, outline.mask_view(), rm);
                    true
                } else {
                    false
                };
                if masked {
                    outline.composite(gpu, &frame.view, [1.0, 1.0, 1.0, 1.0], 1.3);
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
        if want_save_project {
            if let Err(e) =
                floptle_scene::save_project(&self.project, std::path::Path::new(PROJECT_PATH))
            {
                eprintln!("  save project failed: {e}");
            }
        }

        // ---- apply UI commands (gpu/egui borrows have ended; `self` is free) ----
        if cmd.close_menu {
            self.context_menu = None;
        }
        if cmd.undo {
            self.undo();
        }
        if cmd.redo {
            self.redo();
        }
        if cmd.copy {
            self.copy_selected();
        }
        if cmd.paste {
            self.paste();
        }
        if cmd.duplicate {
            self.duplicate_selected();
        }
        if cmd.delete {
            self.delete_selected();
        }
        if let Some(m) = cmd.add {
            let name = match &m {
                MatterDoc::Primitive { shape: ShapeDoc::Sphere, .. } => "Sphere",
                MatterDoc::Primitive { shape: ShapeDoc::Cube, .. } => "Cube",
                MatterDoc::Blob { .. } => "Blob",
            };
            self.add_node(name, m);
        }
        if cmd.inspector_changed {
            self.begin_edit();
        }
    }

    /// Switch the active tool and cancel any in-progress gizmo drag.
    fn set_tool(&mut self, tool: Tool) {
        self.tool = tool;
        self.grabbed = None;
        self.drag = None;
    }

    // ---- selection ----------------------------------------------------------
    /// The entity the gizmo + inspector act on (the most recently selected).
    fn primary(&self) -> Option<Entity> {
        self.selection.last().copied()
    }
    fn select_single(&mut self, e: Entity) {
        self.selection.clear();
        self.selection.push(e);
    }
    fn select_toggle(&mut self, e: Entity) {
        if let Some(i) = self.selection.iter().position(|&x| x == e) {
            self.selection.remove(i);
        } else {
            self.selection.push(e);
        }
    }
    fn select_all(&mut self) {
        self.selection = self.world.query::<Matter>().map(|(e, _)| e).collect();
    }
    /// Selected entities that are real Matter nodes (excludes the Lighting node).
    fn selected_matter(&self) -> Vec<Entity> {
        self.selection.iter().copied().filter(|&e| self.world.get::<Matter>(e).is_some()).collect()
    }

    // ---- undo / redo (whole-scene snapshots) --------------------------------
    fn snapshot(&self) -> SceneDoc {
        floptle_scene::to_doc(self.scene_name.clone(), &self.world)
    }
    fn push_history(&mut self, snap: SceneDoc) {
        self.history.redo.clear();
        self.history.undo.push(snap);
        while self.history.undo.len() > self.history.max {
            self.history.undo.remove(0);
        }
    }
    /// Record the current scene as an undo point (call BEFORE a discrete edit).
    fn record(&mut self) {
        let s = self.snapshot();
        self.push_history(s);
    }
    /// Open an edit session for undo coalescing (gizmo/inspector drag = one step),
    /// using this frame's pre-edit snapshot.
    fn begin_edit(&mut self) {
        if !self.editing {
            if let Some(snap) = self.frame_snapshot.take() {
                self.push_history(snap);
            }
            self.editing = true;
        }
    }
    fn restore(&mut self, doc: SceneDoc) {
        self.world = World::new();
        floptle_scene::spawn_into(&doc, &mut self.world);
        self.selection.clear();
        self.grabbed = None;
        self.drag = None;
    }
    fn undo(&mut self) {
        if let Some(prev) = self.history.undo.pop() {
            let cur = self.snapshot();
            self.history.redo.push(cur);
            self.restore(prev);
        }
    }
    fn redo(&mut self) {
        if let Some(next) = self.history.redo.pop() {
            let cur = self.snapshot();
            self.history.undo.push(cur);
            self.restore(next);
        }
    }

    // ---- node create / delete / clipboard -----------------------------------
    fn node_of(&self, e: Entity) -> Option<NodeDoc> {
        let matter = self.world.get::<Matter>(e)?;
        let transform =
            self.world.get::<Transform>(e).map(TransformDoc::from).unwrap_or_default();
        let name = self.world.get::<Name>(e).map(|n| n.0.clone()).unwrap_or_else(|| "node".into());
        Some(NodeDoc { name, transform, matter: MatterDoc::from(matter) })
    }
    fn spawn_node(&mut self, node: &NodeDoc) -> Entity {
        let e = self.world.spawn();
        self.world.insert(e, node.transform.to_transform());
        self.world.insert(e, Name(node.name.clone()));
        self.world.insert(e, node.matter.to_matter());
        e
    }
    /// Spawn a new node ~5 units in front of the camera, and select it.
    fn add_node(&mut self, name: &str, matter: MatterDoc) {
        self.record();
        let cam = self.camera.render_camera();
        let mut pos = cam.world_position + (cam.rotation * Vec3::NEG_Z * 5.0).as_dvec3();
        if self.grid.snap {
            pos = snap_dvec3(pos, self.grid.size as f64);
        }
        let node = NodeDoc {
            name: name.into(),
            transform: TransformDoc { translation: [pos.x, pos.y, pos.z], ..Default::default() },
            matter,
        };
        let e = self.spawn_node(&node);
        self.select_single(e);
    }
    fn delete_selected(&mut self) {
        let targets = self.selected_matter();
        if targets.is_empty() {
            return;
        }
        self.record();
        for e in targets {
            self.world.despawn(e);
        }
        self.selection.clear();
        self.grabbed = None;
        self.drag = None;
    }
    fn copy_selected(&mut self) {
        let nodes: Vec<NodeDoc> =
            self.selected_matter().iter().filter_map(|&e| self.node_of(e)).collect();
        if !nodes.is_empty() {
            self.clipboard = nodes;
        }
    }
    /// Spawn the given nodes (offset slightly) and select them — used by paste/dup.
    fn spawn_offset(&mut self, nodes: Vec<NodeDoc>) {
        if nodes.is_empty() {
            return;
        }
        self.record();
        self.selection.clear();
        for mut node in nodes {
            node.transform.translation[0] += 0.5;
            node.transform.translation[2] += 0.5;
            let e = self.spawn_node(&node);
            self.selection.push(e);
        }
    }
    fn paste(&mut self) {
        let nodes = self.clipboard.clone();
        self.spawn_offset(nodes);
    }
    fn duplicate_selected(&mut self) {
        let nodes: Vec<NodeDoc> =
            self.selected_matter().iter().filter_map(|&e| self.node_of(e)).collect();
        self.spawn_offset(nodes);
    }

    /// Pick the nearest selectable entity under a viewport cursor (physical px) by
    /// casting a ray against each object's bounding sphere. `None` = empty space.
    fn pick(&self, cursor: Vec2) -> Option<Entity> {
        let gpu = self.gpu.as_ref()?;
        let (w, h) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
        let cam = self.camera.render_camera();
        let inv = cam.view_proj(w / h).inverse();
        // Camera-relative ray (the world is offset to the camera, ADR-0015).
        let ndc = Vec2::new(cursor.x / w * 2.0 - 1.0, 1.0 - cursor.y / h * 2.0);
        let near = inv * Vec4::new(ndc.x, ndc.y, 0.0, 1.0);
        let far = inv * Vec4::new(ndc.x, ndc.y, 1.0, 1.0);
        let ro = near.truncate() / near.w;
        let rd = (far.truncate() / far.w - ro).normalize();

        let mut best: Option<(Entity, f32)> = None;
        for (e, m) in self.world.query::<Matter>() {
            let Some(t) = self.world.get::<Transform>(e) else { continue };
            let center_rel = (t.translation - cam.world_position).as_vec3();
            if let Some(hit) = ray_sphere(ro, rd, center_rel, bounding_radius(m, t)) {
                if best.is_none_or(|(_, bt)| hit < bt) {
                    best = Some((e, hit));
                }
            }
        }
        best.map(|(e, _)| e)
    }

    /// Apply a gizmo drag for the grabbed handle, as an ABSOLUTE transform from the
    /// start-of-drag snapshot (no per-event accumulation → no drift).
    fn gizmo_drag(&mut self) {
        let (Some(drag), Some(cursor), Some(e)) = (self.drag, self.cursor, self.primary()) else {
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
        let (snap, step) = (self.grid.snap, self.grid.size as f64);

        match self.tool {
            Tool::Move => {
                if let Some(i) = handle.axis_index() {
                    let dir = local_axis(start.rotation, i);
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
                        let mut p = start.translation + (dir * units).as_dvec3();
                        if snap {
                            p = snap_dvec3(p, step);
                        }
                        t.translation = p;
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
                        let mut p = start.translation + mv.as_dvec3();
                        if snap {
                            p = snap_dvec3(p, step);
                        }
                        t.translation = p;
                    }
                }
            }
            Tool::Rotate => {
                if let Some(i) = handle.axis_index() {
                    // Rotate about the object's local axis (in world space).
                    let dir = local_axis(start.rotation, i);
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
                } else {
                    // Center handle: free / trackball rotate about the camera axes —
                    // drag horizontally to spin about camera-up, vertically about
                    // camera-right.
                    let cam_right = cam.rotation * Vec3::X;
                    let cam_up = cam.rotation * Vec3::Y;
                    let q = Quat::from_axis_angle(cam_up, cursor_delta.x * TRACKBALL_SENS)
                        * Quat::from_axis_angle(cam_right, cursor_delta.y * TRACKBALL_SENS);
                    if let Some(t) = self.world.get_mut::<Transform>(e) {
                        t.rotation = (q * start.rotation).normalize();
                    }
                }
            }
            Tool::Scale => {
                if let Some(i) = handle.axis_index() {
                    let dir = local_axis(start.rotation, i);
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
        let doc = floptle_scene::to_doc(self.scene_name.clone(), &self.world);
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
        lighting: LightDoc::default(),
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
