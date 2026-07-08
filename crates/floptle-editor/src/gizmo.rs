//! The viewport transform gizmo: screen-space handles for Move / Rotate /
//! Scale, hand-painted with egui's painter and hit-tested in physical pixels.
//!
//! The geometry (axis tips, rotation rings) is projected from the selected
//! object's Transform once per frame into `GizmoFrame`, so window/device
//! events can hit-test the cursor cheaply. Dragging applies an absolute
//! transform from a start-of-drag snapshot (no per-event accumulation → no
//! drift). It only PAINTS — it never registers an egui widget — so it never
//! steals input from panels or the RMB fly-camera.

use floptle_core::math::{DVec3, Mat4, Quat, Vec2, Vec3};
use floptle_core::transform::Transform;
use floptle_core::{Entity, World};

use crate::viz::project;

/// Handle length on screen, in physical pixels (kept roughly constant with depth).
pub(crate) const GIZMO_PX: f32 = 90.0;
/// Cursor-to-handle pick radius, physical pixels.
pub(crate) const HANDLE_PX: f32 = 12.0;
/// Axis-scale drag sensitivity (scale factor per pixel along the axis).
pub(crate) const SCALE_SENS: f32 = 0.01;
/// Screen radius (px) of the Rotate tool's center trackball ring.
pub(crate) const CENTER_RING_PX: f32 = 52.0;
/// Trackball free-rotate sensitivity (radians per pixel).
pub(crate) const TRACKBALL_SENS: f32 = 0.01;

/// The active editing tool. Bound to number keys 1-4 (5-9 reserved).
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Tool {
    #[default]
    Select,
    Move,
    Rotate,
    Scale,
    /// Terrain sculpt/paint brush (LMB-drag edits the terrain field).
    Sculpt,
    /// Bounds box: drag a face to stretch the object toward that side (the
    /// opposite face stays put — scale + offset in one gesture). The main
    /// arranging tool for UI elements; works on 3D shapes too (pull a cube
    /// into a floor).
    Rect,
}

impl Tool {
    pub(crate) fn from_digit(n: u32) -> Option<Tool> {
        match n {
            1 => Some(Tool::Select),
            2 => Some(Tool::Move),
            3 => Some(Tool::Rotate),
            4 => Some(Tool::Scale),
            5 => Some(Tool::Sculpt),
            6 => Some(Tool::Rect),
            _ => None, // 7-9 reserved for future tools
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Tool::Select => "select",
            Tool::Move => "move",
            Tool::Rotate => "rotate",
            Tool::Scale => "scale",
            Tool::Sculpt => "sculpt",
            Tool::Rect => "rect",
        }
    }
}

/// Which part of the gizmo the cursor is over / grabbed. An axis handle's meaning
/// depends on the active `Tool` (move along / rotate about / scale along it).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Handle {
    AxisX,
    AxisY,
    AxisZ,
    /// Rect tool: the −X/−Y/−Z faces (the +axis faces reuse AxisX/Y/Z).
    AxisXN,
    AxisYN,
    AxisZN,
    Center,
}

impl Handle {
    /// Index into the world basis (X=0, Y=1, Z=2), or `None` for the center.
    pub(crate) fn axis_index(self) -> Option<usize> {
        match self {
            Handle::AxisX | Handle::AxisXN => Some(0),
            Handle::AxisY | Handle::AxisYN => Some(1),
            Handle::AxisZ | Handle::AxisZN => Some(2),
            Handle::Center => None,
        }
    }

    /// Which side of the axis a Rect face handle sits on (+1 / −1).
    pub(crate) fn sign(self) -> f32 {
        match self {
            Handle::AxisXN | Handle::AxisYN | Handle::AxisZN => -1.0,
            _ => 1.0,
        }
    }
}

/// Cached, projected gizmo geometry for the current frame (all in physical pixels).
pub(crate) struct GizmoFrame {
    pub(crate) center: Vec2,
    /// Local-axis arrow tips; `None` for an axis that projects behind the camera.
    /// For the Rect tool these are the +axis FACE centers of the bounds box.
    pub(crate) tips: [Option<Vec2>; 3],
    /// Rect tool: the −axis face centers.
    pub(crate) neg_tips: [Option<Vec2>; 3],
    /// Rect tool: the projected bounds-box edges (12 segments).
    pub(crate) box_edges: Vec<[Vec2; 2]>,
    /// Rotation-ring polylines, one per local axis (only filled for the Rotate tool).
    pub(crate) ring_pts: [Vec<Vec2>; 3],
    /// A flat screen-space ring around the center: the free/trackball handle for
    /// Rotate, drawn so the center handle is grabbable (Move/Scale use a box).
    pub(crate) center_ring: Vec<Vec2>,
    /// Which handle the cursor is hovering this frame, if any.
    pub(crate) hovered: Option<Handle>,
}

/// A start-of-drag snapshot, so drags apply an absolute transform (no drift).
#[derive(Clone, Copy)]
pub(crate) struct DragState {
    pub(crate) handle: Handle,
    /// The entity this snapshot belongs to — guards against the selection
    /// changing mid-drag and applying the wrong object's start transform.
    pub(crate) entity: Entity,
    pub(crate) start_xf: Transform,
    pub(crate) cursor_start: Vec2,
}

/// World basis vector for axis `i` (X=0, Y=1, Z=2).
pub(crate) fn axis_world(i: usize) -> Vec3 {
    [Vec3::X, Vec3::Y, Vec3::Z][i]
}

/// The object's LOCAL axis `i` expressed in world space (so the gizmo aligns with
/// the object's current orientation, not the world frame).
pub(crate) fn local_axis(rot: Quat, i: usize) -> Vec3 {
    rot * axis_world(i)
}

pub(crate) fn handle_for_axis(i: usize) -> Handle {
    [Handle::AxisX, Handle::AxisY, Handle::AxisZ][i]
}

pub(crate) fn seg_dist(p: Vec2, a: Vec2, b: Vec2) -> f32 {
    let ab = b - a;
    let len2 = ab.length_squared();
    let t = if len2 < 1e-6 { 0.0 } else { ((p - a).dot(ab) / len2).clamp(0.0, 1.0) };
    (p - (a + ab * t)).length()
}

/// Snap each component of a world position to a grid `step` (no-op if step ≤ 0).
pub(crate) fn ray_sphere(ro: Vec3, rd: Vec3, center: Vec3, radius: f32) -> Option<f32> {
    let oc = ro - center;
    let a = rd.dot(rd);
    let b = 2.0 * oc.dot(rd);
    let c = oc.length_squared() - radius * radius;
    let disc = b * b - 4.0 * a * c;
    if disc < 0.0 {
        return None;
    }
    let s = disc.sqrt();
    let t0 = (-b - s) / (2.0 * a);
    if t0 > 1e-3 {
        return Some(t0);
    }
    let t1 = (-b + s) / (2.0 * a); // origin inside the sphere
    (t1 > 1e-3).then_some(t1)
}

/// Nearest positive ray–AABB hit `t` for a box centered at the origin with the given
/// `half` extent (slab method; `rd` need not be unit).
pub(crate) fn ray_aabb(ro: Vec3, rd: Vec3, half: f32) -> Option<f32> {
    let inv = Vec3::ONE / rd; // 0 components ⏵ ±inf, handled by the min/max
    let t1 = (Vec3::splat(-half) - ro) * inv;
    let t2 = (Vec3::splat(half) - ro) * inv;
    let near = t1.min(t2).max_element();
    let far = t1.max(t2).min_element();
    if near <= far && far > 1e-3 {
        Some(near.max(1e-3))
    } else {
        None
    }
}

/// Build the gizmo geometry for the selected entity and hit-test the cursor.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_gizmo(
    tool: Tool,
    selection: Option<Entity>,
    world: &World,
    cursor: Option<Vec2>,
    cam_world: DVec3,
    vp: Mat4,
    w: f32,
    h: f32,
    rect_half: Option<Vec3>,
) -> Option<GizmoFrame> {
    if tool == Tool::Select || tool == Tool::Sculpt {
        return None;
    }
    let e = selection?;
    // World transform, so the gizmo sits on the node's actual (parented) placement.
    let t = floptle_core::world_transform(world, e);
    let center = project(t.translation, cam_world, vp, w, h)?;
    let rot = t.rotation;

    if tool == Tool::Rect {
        // Bounds box: face handles at ±half along the object's local axes.
        let base = rect_half?;
        let half = [
            (base.x * t.scale.x.abs()).max(1e-3),
            (base.y * t.scale.y.abs()).max(1e-3),
            (base.z * t.scale.z.abs()).max(1e-3),
        ];
        let mut tips = [None; 3];
        let mut neg_tips = [None; 3];
        for i in 0..3 {
            let d = (local_axis(rot, i) * half[i]).as_dvec3();
            tips[i] = project(t.translation + d, cam_world, vp, w, h);
            neg_tips[i] = project(t.translation - d, cam_world, vp, w, h);
        }
        // The 12 box edges, projected.
        let corner = |sx: f32, sy: f32, sz: f32| {
            t.translation
                + (local_axis(rot, 0) * (half[0] * sx)
                    + local_axis(rot, 1) * (half[1] * sy)
                    + local_axis(rot, 2) * (half[2] * sz))
                    .as_dvec3()
        };
        let signs = [
            [-1.0f32, -1.0, -1.0], [1.0, -1.0, -1.0], [1.0, 1.0, -1.0], [-1.0, 1.0, -1.0],
            [-1.0, -1.0, 1.0], [1.0, -1.0, 1.0], [1.0, 1.0, 1.0], [-1.0, 1.0, 1.0],
        ];
        const EDGES: [(usize, usize); 12] = [
            (0, 1), (1, 2), (2, 3), (3, 0),
            (4, 5), (5, 6), (6, 7), (7, 4),
            (0, 4), (1, 5), (2, 6), (3, 7),
        ];
        let pts: Vec<Option<Vec2>> = signs
            .iter()
            .map(|s| project(corner(s[0], s[1], s[2]), cam_world, vp, w, h))
            .collect();
        let mut box_edges = Vec::new();
        for (a, b) in EDGES {
            if let (Some(pa), Some(pb)) = (pts[a], pts[b]) {
                box_edges.push([pa, pb]);
            }
        }
        let hovered = cursor.and_then(|c| {
            let mut cands: Vec<(Handle, f32)> = Vec::new();
            for i in 0..3 {
                if let Some(p) = tips[i] {
                    cands.push((handle_for_axis(i), (c - p).length()));
                }
                if let Some(p) = neg_tips[i] {
                    cands.push((
                        [Handle::AxisXN, Handle::AxisYN, Handle::AxisZN][i],
                        (c - p).length(),
                    ));
                }
            }
            cands
                .into_iter()
                .filter(|(_, d)| *d <= HANDLE_PX)
                .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
                .map(|(h, _)| h)
        });
        return Some(GizmoFrame {
            center,
            tips,
            neg_tips,
            box_edges,
            ring_pts: [Vec::new(), Vec::new(), Vec::new()],
            center_ring: Vec::new(),
            hovered,
        });
    }

    // Pixel-constant handle length: world units that subtend ~GIZMO_PX at this depth
    // (60° vertical fov). Clamp the near distance so a close object doesn't explode.
    let dist = (t.translation - cam_world).length().max(0.4) as f32;
    let axis_len = GIZMO_PX * 2.0 * dist * (30f32.to_radians()).tan() / h;

    // Tips follow the object's LOCAL axes, so the gizmo aligns with its orientation.
    let mut tips = [None; 3];
    for (i, tip) in tips.iter_mut().enumerate() {
        let tip_world = t.translation + (local_axis(rot, i) * axis_len).as_dvec3();
        *tip = project(tip_world, cam_world, vp, w, h);
    }

    // Rotation rings live in the planes spanned by the object's local axes.
    let mut ring_pts: [Vec<Vec2>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let mut center_ring: Vec<Vec2> = Vec::new();
    if tool == Tool::Rotate {
        const N: usize = 48;
        for (i, ring) in ring_pts.iter_mut().enumerate() {
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
            *ring = pts;
        }
        // A flat screen-space trackball ring around the center — the free-rotate handle.
        const M: usize = 40;
        for k in 0..=M {
            let a = (k as f32) / (M as f32) * std::f32::consts::TAU;
            center_ring.push(center + Vec2::new(a.cos(), a.sin()) * CENTER_RING_PX);
        }
    }

    let hovered = cursor.and_then(|c| hit_test(tool, c, center, &tips, &ring_pts, &center_ring));
    Some(GizmoFrame {
        center,
        tips,
        neg_tips: [None; 3],
        box_edges: Vec::new(),
        ring_pts,
        center_ring,
        hovered,
    })
}

/// Nearest gizmo handle to the cursor within `HANDLE_PX`, if any.
pub(crate) fn hit_test(
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
            for (i, tip) in tips.iter().enumerate() {
                if let Some(tip) = *tip {
                    cands.push((handle_for_axis(i), seg_dist(cursor, center, tip)));
                }
            }
            cands.push((Handle::Center, (cursor - center).length()));
        }
        Tool::Rotate => {
            for (i, ring) in rings.iter().enumerate() {
                cands.push((handle_for_axis(i), ring_dist(ring)));
            }
            // The trackball ring (free rotate) — only when not closer to an axis ring.
            cands.push((Handle::Center, ring_dist(center_ring)));
        }
        Tool::Select | Tool::Sculpt | Tool::Rect => {} // Rect hit-tests in build_gizmo
    }
    cands
        .into_iter()
        .filter(|(_, d)| *d <= HANDLE_PX)
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(h, _)| h)
}

/// Brighten a handle color toward white when it is hovered or grabbed.
pub(crate) fn brighten(c: egui::Color32, on: bool) -> egui::Color32 {
    if !on {
        return c;
    }
    let mix = |x: u8| ((x as u16 + 255) / 2) as u8;
    egui::Color32::from_rgb(mix(c.r()), mix(c.g()), mix(c.b()))
}

/// A small filled arrowhead at `to`, pointing away from `from`.
pub(crate) fn arrow_head(painter: &egui::Painter, from: egui::Pos2, to: egui::Pos2, col: egui::Color32) {
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
pub(crate) fn paint_gizmo(painter: &egui::Painter, g: &GizmoFrame, tool: Tool, grabbed: Option<Handle>, ppp: f32) {
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
            for (i, (tip, col)) in g.tips.iter().zip(axis_col).enumerate() {
                if let Some(tip) = *tip {
                    let on = active(handle_for_axis(i));
                    let col = brighten(col, on);
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
            for (i, (tip, col)) in g.tips.iter().zip(axis_col).enumerate() {
                if let Some(tip) = *tip {
                    let on = active(handle_for_axis(i));
                    let col = brighten(col, on);
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
            for (i, (ring, col)) in g.ring_pts.iter().zip(axis_col).enumerate() {
                let on = active(handle_for_axis(i));
                let col = brighten(col, on);
                let pts: Vec<Pos2> = ring.iter().map(|v| pt(*v)).collect();
                if pts.len() >= 2 {
                    painter.line(pts, Stroke::new(if on { 3.5 } else { 2.0 }, col));
                }
            }
            painter.circle_filled(center, 3.0, Color32::from_gray(200));
        }
        Tool::Rect => {
            // Bounds box + face squares (axis-colored; hover brightens).
            for e in &g.box_edges {
                painter.line_segment(
                    [pt(e[0]), pt(e[1])],
                    Stroke::new(1.5, Color32::from_rgba_unmultiplied(220, 220, 230, 160)),
                );
            }
            for i in 0..3 {
                let col = axis_col[i];
                for (tip, hnd) in [
                    (g.tips[i], handle_for_axis(i)),
                    (g.neg_tips[i], [Handle::AxisXN, Handle::AxisYN, Handle::AxisZN][i]),
                ] {
                    if let Some(tip) = tip {
                        let on = active(hnd);
                        painter.rect_filled(
                            egui::Rect::from_center_size(
                                pt(tip),
                                egui::vec2(if on { 11.0 } else { 9.0 }, if on { 11.0 } else { 9.0 }),
                            ),
                            1.5,
                            brighten(col, on),
                        );
                    }
                }
            }
        }
        Tool::Select | Tool::Sculpt => {}
    }
}
