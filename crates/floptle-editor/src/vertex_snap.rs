//! Vertex snapping: hold **V** with the Move or Place tool, hover a corner of
//! the selection, and drag — the corner lands exactly on the nearest corner of
//! whatever is under the cursor. Modular kits are built so their pieces meet
//! corner to corner; this is how they are put together without measuring.
//!
//! Everything is picked on screen, in pixels, because that is what the eye is
//! aiming at: the vertex nearest the cursor is the one being pointed at, at
//! any distance from the camera.

use crate::Editor;
use floptle_core::Entity;
use floptle_core::Matter;
use floptle_core::math::{DVec3, Mat4, Vec2, Vec3};
use floptle_core::transform::Transform;
use std::collections::HashSet;

/// How near the cursor (physical px) a vertex must be to be taken.
pub(crate) const SNAP_PX: f32 = 28.0;

/// A vertex-snap drag in progress.
#[derive(Clone, Debug)]
pub(crate) struct VertexDrag {
    pub(crate) entity: Entity,
    pub(crate) start_xf: Transform,
    /// The grabbed corner, world space, where it was at the press.
    pub(crate) source: DVec3,
    /// What moves with the drag, and so is never a target.
    pub(crate) skip: HashSet<Entity>,
    /// The corner the source is on now, when it is on one.
    pub(crate) target: Option<DVec3>,
}

/// The vertex among `verts` (world space) that lands nearest `cursor` on
/// screen, within `max_px`: `(vertex, pixel distance)`.
pub(crate) fn nearest_on_screen(
    verts: impl IntoIterator<Item = DVec3>,
    cursor: Vec2,
    max_px: f32,
    to_screen: impl Fn(DVec3) -> Option<Vec2>,
) -> Option<(DVec3, f32)> {
    let mut best: Option<(DVec3, f32)> = None;
    for v in verts {
        let Some(s) = to_screen(v) else { continue };
        let d = (s - cursor).length();
        if d <= max_px && best.is_none_or(|b| d < b.1) {
            best = Some((v, d));
        }
    }
    best
}

impl Editor {
    /// World → physical-pixel projection for this frame's camera.
    fn screen_projector(&self) -> Option<impl Fn(DVec3) -> Option<Vec2> + use<>> {
        let gpu = self.gpu.as_ref()?;
        let (w, h) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
        let cam = self.camera.render_camera();
        let vp: Mat4 = cam.view_proj(w / h);
        let at = cam.world_position;
        Some(move |p: DVec3| crate::viz::project(p, at, vp, w, h))
    }

    /// A node's own vertices, world space.
    pub(crate) fn node_world_verts(&mut self, e: Entity) -> Vec<DVec3> {
        let m = floptle_core::world_transform(&self.world, e).world_matrix();
        let local: Vec<Vec3> = match self.world.get::<Matter>(e) {
            // From the store: the triangle cache keeps the geometry it was
            // first built from while the mesh is edited.
            Some(Matter::MapMesh { id }) => self.maps.meshes.get(id).map(|m| m.verts.clone()).unwrap_or_default(),
            Some(Matter::Mesh { .. } | Matter::Primitive { .. }) => {
                let Some(key) = self.ensure_paint_mesh_pub(e) else { return Vec::new() };
                self.paint_meshes
                    .get(&key)
                    .map(|parts| parts.iter().flat_map(|p| p.verts.iter().map(|v| Vec3::from(v.pos))).collect())
                    .unwrap_or_default()
            }
            _ => Vec::new(),
        };
        local.into_iter().map(|v| m.transform_point3(v.as_dvec3())).collect()
    }

    /// The vertex of `nodes` nearest `cursor` on screen, within [`SNAP_PX`].
    pub(crate) fn nearest_vertex_of(&mut self, nodes: &HashSet<Entity>, cursor: Vec2) -> Option<DVec3> {
        let project = self.screen_projector()?;
        let mut best: Option<(DVec3, f32)> = None;
        for &e in nodes {
            let verts = self.node_world_verts(e);
            if let Some(hit) = nearest_on_screen(verts, cursor, SNAP_PX, &project)
                && best.is_none_or(|b| hit.1 < b.1)
            {
                best = Some(hit);
            }
        }
        best.map(|(v, _)| v)
    }

    /// The vertex of any node but `skip` nearest `cursor` on screen. Only the
    /// nodes whose on-screen bounds reach the cursor have their vertices read.
    pub(crate) fn snap_target(&mut self, cursor: Vec2, skip: &HashSet<Entity>) -> Option<DVec3> {
        let project = self.screen_projector()?;
        let candidates: Vec<Entity> = self
            .world
            .query::<Matter>()
            .filter(|(e, m)| {
                !skip.contains(e) && matches!(m, Matter::Mesh { .. } | Matter::Primitive { .. } | Matter::MapMesh { .. })
            })
            .map(|(e, _)| e)
            .collect();
        let mut near: HashSet<Entity> = HashSet::new();
        for e in candidates {
            let corners = self.world_corners(&HashSet::from([e]));
            let pts: Vec<Vec2> = corners.into_iter().filter_map(&project).collect();
            if pts.len() < 8 {
                // Partly behind the camera: its box cannot be trusted on
                // screen, so read its vertices, which are projected one by one.
                near.insert(e);
                continue;
            }
            let lo = pts.iter().fold(Vec2::splat(f32::MAX), |a, p| a.min(*p));
            let hi = pts.iter().fold(Vec2::splat(f32::MIN), |a, p| a.max(*p));
            if cursor.cmpge(lo - SNAP_PX).all() && cursor.cmple(hi + SNAP_PX).all() {
                near.insert(e);
            }
        }
        self.nearest_vertex_of(&near, cursor)
    }

    /// The selection's roots and everything under them.
    fn moving_set(&self) -> (Vec<Entity>, HashSet<Entity>) {
        let roots: Vec<Entity> = self
            .selection
            .iter()
            .copied()
            .filter(|&o| !self.selection.iter().any(|&a| a != o && self.is_descendant(o, a)))
            .collect();
        let all = self.subtree_of(&roots);
        (roots, all)
    }

    /// With V held over the viewport: the selection's corner under the cursor,
    /// which a press would grab.
    pub(crate) fn vertex_snap_hover(&mut self) -> Option<DVec3> {
        let cursor = self.cursor?;
        let (_, all) = self.moving_set();
        self.nearest_vertex_of(&all, cursor)
    }

    /// A press with V held: grab the hovered corner. False when there is none.
    pub(crate) fn vertex_snap_press(&mut self) -> bool {
        let (Some(source), Some(e)) = (self.vertex_snap_hover(), self.primary()) else { return false };
        let (roots, skip) = self.moving_set();
        self.begin_edit();
        self.drag_group = roots
            .iter()
            .copied()
            .filter(|&o| o != e)
            .map(|o| (o, floptle_core::world_transform(&self.world, o)))
            .collect();
        self.vertex_drag = Some(VertexDrag {
            entity: e,
            start_xf: floptle_core::world_transform(&self.world, e),
            source,
            skip,
            target: None,
        });
        true
    }

    /// Follow the cursor: the grabbed corner sits on the nearest corner under
    /// the cursor, or, with none near, follows the cursor in the plane through
    /// the corner that faces the camera.
    pub(crate) fn vertex_drag_update(&mut self) {
        let (Some(mut drag), Some(cursor)) = (self.vertex_drag.take(), self.cursor) else { return };
        drag.target = self.snap_target(cursor, &drag.skip);
        let to = drag.target.or_else(|| {
            let cam = self.camera.render_camera();
            let (ro, rd) = self.cursor_ray(cursor)?;
            let origin = (drag.source - cam.world_position).as_vec3();
            let facing = cam.rotation * Vec3::NEG_Z;
            let denom = rd.dot(facing);
            (denom.abs() > 1e-6)
                .then(|| cam.world_position + (ro + rd * ((origin - ro).dot(facing) / denom)).as_dvec3())
        });
        if let Some(to) = to {
            let xf = Transform { translation: drag.start_xf.translation + (to - drag.source), ..drag.start_xf };
            self.set_world_transform(drag.entity, xf);
            self.apply_group_transform(drag.start_xf, xf);
        }
        self.vertex_drag = Some(drag);
    }

    /// Mark the corner V would grab, and during a snap drag the corner it is
    /// on, joined by a line.
    #[cfg(feature = "editor-ui")]
    pub(crate) fn paint_vertex_snap(&mut self, ctx: &egui::Context) {
        let Some(project) = self.screen_projector() else { return };
        let (from, to) = match &self.vertex_drag {
            Some(d) => {
                let now = floptle_core::world_transform(&self.world, d.entity).translation;
                (Some(d.source + (now - d.start_xf.translation)), d.target)
            }
            None => (self.vertex_snap_hover(), None),
        };
        let ppp = ctx.pixels_per_point();
        let pt = |v: DVec3| project(v).map(|s| egui::pos2(s.x / ppp, s.y / ppp));
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("vertex_snap")));
        let mark = |p: egui::Pos2, col: egui::Color32| {
            let r = egui::Rect::from_center_size(p, egui::vec2(9.0, 9.0));
            painter.rect_filled(r.expand(1.5), 1.0, egui::Color32::from_black_alpha(150));
            painter.rect_filled(r, 1.0, col);
        };
        if let Some(a) = from.and_then(pt) {
            mark(a, egui::Color32::from_rgb(235, 225, 150));
            if let Some(b) = to.and_then(pt) {
                painter.line_segment([a, b], egui::Stroke::new(1.5, egui::Color32::from_rgb(120, 230, 140)));
                mark(b, egui::Color32::from_rgb(120, 230, 140));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vertex nearest the cursor on screen is the one taken, and only
    /// within reach — a corner across the screen is not pointed at.
    #[test]
    fn the_nearest_vertex_on_screen_within_reach_is_taken() {
        // An orthographic stand-in: x and y are the screen, z is depth.
        let project = |v: DVec3| Some(Vec2::new(v.x as f32, v.y as f32));
        let verts = [DVec3::new(10.0, 10.0, 5.0), DVec3::new(30.0, 12.0, 1.0), DVec3::new(200.0, 0.0, 0.0)];
        let hit = nearest_on_screen(verts, Vec2::new(25.0, 12.0), SNAP_PX, project);
        assert_eq!(hit.map(|h| h.0), Some(verts[1]), "nearest on screen, whatever its depth");
        assert!(nearest_on_screen(verts, Vec2::new(120.0, 80.0), SNAP_PX, project).is_none());
    }

    /// The corners snapping aims at are where the node draws them: its
    /// placement, turn and scale applied.
    #[test]
    fn a_nodes_corners_are_read_in_world_space() {
        let mut ed = Editor::default();
        let e = ed.world.spawn();
        ed.world.insert(e, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [1.0; 3] });
        ed.world.insert(
            e,
            Transform {
                translation: DVec3::new(5.0, 0.0, 0.0),
                rotation: floptle_core::math::Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
                scale: Vec3::splat(2.0),
            },
        );
        let verts = ed.node_world_verts(e);
        assert!(!verts.is_empty());
        let h = 2.0 * crate::matter_catalog::PRIMITIVE_HALF as f64;
        let want = DVec3::new(5.0 + h, h, h);
        assert!(verts.iter().any(|v| (*v - want).length() < 1e-4), "no corner at {want}");
        assert!(verts.iter().all(|v| (v.x - 5.0).abs() <= h + 1e-4), "every corner is around the node");
    }
}
