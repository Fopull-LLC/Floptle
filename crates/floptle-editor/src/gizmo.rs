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

use crate::project;

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
}

impl Tool {
    pub(crate) fn from_digit(n: u32) -> Option<Tool> {
        match n {
            1 => Some(Tool::Select),
            2 => Some(Tool::Move),
            3 => Some(Tool::Rotate),
            4 => Some(Tool::Scale),
            5 => Some(Tool::Sculpt),
            _ => None, // 6-9 reserved for future tools
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Tool::Select => "select",
            Tool::Move => "move",
            Tool::Rotate => "rotate",
            Tool::Scale => "scale",
            Tool::Sculpt => "sculpt",
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
    Center,
}

impl Handle {
    /// Index into the world basis (X=0, Y=1, Z=2), or `None` for the center.
    pub(crate) fn axis_index(self) -> Option<usize> {
        match self {
            Handle::AxisX => Some(0),
            Handle::AxisY => Some(1),
            Handle::AxisZ => Some(2),
            Handle::Center => None,
        }
    }
}

/// Cached, projected gizmo geometry for the current frame (all in physical pixels).
pub(crate) struct GizmoFrame {
    pub(crate) center: Vec2,
    /// Local-axis arrow tips; `None` for an axis that projects behind the camera.
    pub(crate) tips: [Option<Vec2>; 3],
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
pub(crate) fn build_gizmo(
    tool: Tool,
    selection: Option<Entity>,
    world: &World,
    cursor: Option<Vec2>,
    cam_world: DVec3,
    vp: Mat4,
    w: f32,
    h: f32,
) -> Option<GizmoFrame> {
    if tool == Tool::Select || tool == Tool::Sculpt {
        return None;
    }
    let e = selection?;
    // World transform, so the gizmo sits on the node's actual (parented) placement.
    let t = floptle_core::world_transform(world, e);
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
        Tool::Select | Tool::Sculpt => {}
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
        Tool::Select | Tool::Sculpt => {}
    }
}
