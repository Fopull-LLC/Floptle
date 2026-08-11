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

/// The active editing tool. Bound to number keys 1-8 (9 reserved).
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
    /// Vertex paint brush (LMB-drag paints per-vertex color onto a mesh).
    Paint,
    /// Map-building sub-object editor: select/drag vertices, edges, faces of a
    /// `Matter::MapMesh` node, extrude, assign per-face materials (▦ Model tab).
    MapEdit,
    /// Tile painting: the ◫ Tiles tab's brush, in the Scene view. Which tool the
    /// pointer actually holds (brush, rectangle, bucket, …) is the Tiles tab's
    /// own `TileTool` — this is only "the pointer paints tiles now", the same
    /// relationship `Sculpt` has to the terrain brush.
    Tiles,
}

impl Tool {
    /// Every tool, in KEYBIND order. This is the single source of truth: `from_digit`,
    /// `digit`, and the viewport toolbar all read it, so the toolbar can never again
    /// disagree with the number keys (it used to list Rect before Sculpt while the keys
    /// said otherwise). Add a tool here and it appears, in order, everywhere.
    pub(crate) const ALL: [Tool; 9] = [
        Tool::Select,
        Tool::Move,
        Tool::Rotate,
        Tool::Scale,
        Tool::Sculpt,
        Tool::Rect,
        Tool::Paint,
        Tool::MapEdit,
        Tool::Tiles,
    ];

    pub(crate) fn from_digit(n: u32) -> Option<Tool> {
        // 9 reserved for future tools.
        Self::ALL.get((n as usize).checked_sub(1)?).copied()
    }

    /// The number key that selects this tool (1-based).
    pub(crate) fn digit(self) -> usize {
        Self::ALL.iter().position(|t| *t == self).map_or(0, |i| i + 1)
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Tool::Select => "select",
            Tool::Move => "move",
            Tool::Rotate => "rotate",
            Tool::Scale => "scale",
            Tool::Sculpt => "sculpt",
            Tool::Tiles => "tiles",
            Tool::Rect => "rect",
            Tool::Paint => "paint",
            Tool::MapEdit => "map",
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
    /// Per ring point: is it on the camera-facing half of the sphere?
    ///
    /// Drawing all three rings whole makes a ball of overlapping circles in which
    /// no ring can be told from another, and nothing says which way any of them
    /// faces. Painting only the near half is what turns it back into three
    /// readable arcs — and the near half is also the half you can actually reach,
    /// so the picture stops promising a grab the far side would win.
    ///
    /// Parallel to [`Self::ring_pts`] (built in the same pass, so points dropped
    /// for projecting behind the camera are dropped from both).
    pub(crate) ring_front: [Vec<bool>; 3],
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
    /// For a BONE drag this is the rigged-mesh entity that owns the bone.
    pub(crate) entity: Entity,
    /// `Some(bone_index)` when the gizmo is posing an armature bone (not an ECS
    /// entity). The drag writes the bone's local pose into the open clip instead
    /// of an entity `Transform`.
    pub(crate) bone: Option<usize>,
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

/// [`ray_aabb`] for a box whose half-extents differ per axis — a tilemap is
/// wide and tall and almost flat, and a cube-shaped pick volume around one
/// would swallow everything standing on it.
pub(crate) fn ray_box(ro: Vec3, rd: Vec3, half: Vec3) -> Option<f32> {
    let inv = Vec3::ONE / rd;
    let t1 = (-half - ro) * inv;
    let t2 = (half - ro) * inv;
    let near = t1.min(t2).max_element();
    let far = t1.max(t2).min_element();
    (near <= far && far > 1e-3).then(|| near.max(1e-3))
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
    xf_override: Option<Transform>,
) -> Option<GizmoFrame> {
    if tool == Tool::Select || tool == Tool::Sculpt || tool == Tool::Paint || tool == Tool::Tiles {
        return None;
    }
    // The Map tool only shows a (Move-style) gizmo when a sub-object selection
    // provides its centroid transform; bare map mode has nothing to drag.
    if tool == Tool::MapEdit && xf_override.is_none() {
        return None;
    }
    // Either an explicit world transform — a selected armature BONE, which is not
    // an ECS entity — or the selected entity's world transform (so the gizmo sits
    // on the node's actual, parented placement).
    let t = match xf_override {
        Some(t) => t,
        None => floptle_core::world_transform(world, selection?),
    };
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
            ring_front: [Vec::new(), Vec::new(), Vec::new()],
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
    let mut ring_front: [Vec<bool>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let mut center_ring: Vec<Vec2> = Vec::new();
    if tool == Tool::Rotate {
        const N: usize = 48;
        for (i, ring) in ring_pts.iter_mut().enumerate() {
            let u = local_axis(rot, (i + 1) % 3);
            let v = local_axis(rot, (i + 2) % 3);
            let mut pts = Vec::with_capacity(N + 1);
            let mut front = Vec::with_capacity(N + 1);
            for k in 0..=N {
                let a = (k as f32) / (N as f32) * std::f32::consts::TAU;
                let off = (u * a.cos() + v * a.sin()) * axis_len;
                let p = t.translation + off.as_dvec3();
                if let Some(s) = project(p, cam_world, vp, w, h) {
                    pts.push(s);
                    // Facing us when the outward radial points back toward the
                    // eye. The two are computed in doubles from the same world
                    // positions the point was projected from, so the near/far
                    // split lands exactly on the silhouette.
                    front.push(off.as_dvec3().dot(p - cam_world) < 0.0);
                }
            }
            // A ring whose axis points at the camera is seen face-on, and the
            // test above — which is the EXACT sphere-silhouette test, so it is
            // right about this — puts every one of its points a hair behind the
            // silhouette. Ghosting it whole would fade out the one ring you can
            // see as a full circle and are most likely to be reaching for. Read
            // "no near half at all" as "this ring IS the silhouette" and draw it
            // solid.
            if !front.iter().any(|f| *f) {
                front.fill(true);
            }
            *ring = pts;
            ring_front[i] = front;
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
        ring_front,
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
        Tool::Move | Tool::Scale | Tool::MapEdit => {
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
        Tool::Select | Tool::Sculpt | Tool::Paint | Tool::Tiles | Tool::Rect => {} // Rect hit-tests in build_gizmo
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
        Tool::Move | Tool::MapEdit => {
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
                let front = &g.ring_front[i];
                // The far half in a faint ghost, the near half solid. Dropping
                // the far half entirely would leave three arcs floating with no
                // hint of the sphere they belong to; this way the ring still
                // reads as a ring, and which way it faces is never in doubt.
                let mut run: Vec<Pos2> = Vec::new();
                let mut ghost: Vec<Pos2> = Vec::new();
                let flush = |painter: &egui::Painter, run: &mut Vec<Pos2>, near: bool| {
                    if run.len() >= 2 {
                        let s = if near {
                            Stroke::new(if on { 3.5 } else { 2.0 }, col)
                        } else {
                            Stroke::new(1.0, col.gamma_multiply(0.25))
                        };
                        painter.line(std::mem::take(run), s);
                    } else {
                        run.clear();
                    }
                };
                for (k, v) in ring.iter().enumerate() {
                    let near = front.get(k).copied().unwrap_or(true);
                    if near {
                        flush(painter, &mut ghost, false);
                        run.push(pt(*v));
                    } else {
                        flush(painter, &mut run, true);
                        ghost.push(pt(*v));
                    }
                }
                flush(painter, &mut run, true);
                flush(painter, &mut ghost, false);
            }
            // The object's own axes, named. Three rings tell you the planes you
            // can turn in; they do not tell you which way the object is FACING,
            // which is the thing you actually need when the object is a head, a
            // wing or a gun. A short stub with a letter on it does, and it is the
            // same red/green/blue the ring uses, so the two read as one gizmo.
            for (i, (tip, col)) in g.tips.iter().zip(axis_col).enumerate() {
                let Some(tip) = *tip else { continue };
                let tp = pt(tip);
                let d = tp - center;
                if d.length() < 6.0 {
                    // Pointing at (or away from) the camera — a stub here would be
                    // a dot on the pivot, and the letter would sit on the origin
                    // marker. The ring's own foreshortening already says so.
                    continue;
                }
                let stub = center + d * 0.45;
                let on = active(handle_for_axis(i));
                painter.line_segment([center, stub], Stroke::new(if on { 2.5 } else { 1.5 }, col));
                painter.text(
                    center + d * 0.58,
                    egui::Align2::CENTER_CENTER,
                    ["X", "Y", "Z"][i],
                    egui::FontId::proportional(11.0),
                    col,
                );
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
        Tool::Select | Tool::Sculpt | Tool::Paint | Tool::Tiles => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_core::math::DVec3;
    use floptle_core::transform::Transform;

    /// A camera at the origin looking down −Z, and an object 10 units in front.
    fn setup() -> (World, Entity, DVec3, Mat4) {
        let mut w = World::new();
        let e = w.spawn();
        w.insert(e, Transform { translation: DVec3::new(0.0, 0.0, -10.0), ..Default::default() });
        // Camera-relative projection: the view carries no translation, so the
        // object's world position IS its position relative to the eye (ADR-0015).
        let proj = Mat4::perspective_rh(60f32.to_radians(), 1.0, 0.1, 1000.0);
        (w, e, DVec3::ZERO, proj)
    }

    /// The near/far split is what makes three rings readable instead of a ball of
    /// circles. A ring seen edge-on — one that contains the view direction — must
    /// come out genuinely halved.
    #[test]
    fn a_ring_seen_edge_on_is_split_into_a_near_and_a_far_half() {
        let (world, e, cam, vp) = setup();
        let g = build_gizmo(Tool::Rotate, Some(e), &world, None, cam, vp, 800.0, 800.0, None, None)
            .expect("the rotate gizmo builds");
        // Rings 0 and 1 (local Y/Z and Z/X) both contain the camera's view axis.
        for i in [0usize, 1] {
            let front = &g.ring_front[i];
            assert_eq!(front.len(), g.ring_pts[i].len(), "ring {i}: flags match points");
            let near = front.iter().filter(|f| **f).count();
            let far = front.len() - near;
            assert!(near > 0 && far > 0, "ring {i}: got {near} near / {far} far");
            let skew = (near as f32 - far as f32).abs() / front.len() as f32;
            assert!(skew < 0.35, "ring {i} splits lopsidedly ({near}/{far})");
        }
    }

    /// The ring facing the camera is the one you see as a full circle and the one
    /// you are most likely to reach for. The exact sphere-silhouette test puts all
    /// of it a hair BEHIND the silhouette, so without the degenerate-case rule it
    /// would be the only ring drawn entirely as a ghost — the exact opposite of
    /// what it deserves.
    #[test]
    fn a_ring_seen_face_on_is_drawn_whole() {
        let (world, e, cam, vp) = setup();
        let g = build_gizmo(Tool::Rotate, Some(e), &world, None, cam, vp, 800.0, 800.0, None, None)
            .expect("builds");
        // Ring 2 spans local X/Y — face-on to a camera looking down −Z.
        assert!(
            g.ring_front[2].iter().all(|f| *f),
            "the face-on ring draws solid, not as a ghost"
        );
    }
}
