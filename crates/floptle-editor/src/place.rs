//! The Place tool, and the placement asset drops share with it: put a node on
//! the surface under the cursor, resting on it rather than sunk into it.
//!
//! A node rests on a surface when the deepest point of its geometry (and its
//! children's) along the surface's normal touches the surface. That works the
//! same for a crate on a floor, a painting on a wall and a lamp under a
//! ceiling, and it needs nothing from the node but its triangles.

use crate::Editor;
use floptle_core::Entity;
use floptle_core::Matter;
use floptle_core::math::{DVec3, Quat, Vec2, Vec3};
use floptle_core::transform::Transform;
use std::collections::HashSet;

/// How far (physical px) the cursor must travel before a Place press becomes a
/// drag — so a click that only meant to select never moves anything.
pub(crate) const PLACE_DRAG_PX: f32 = 4.0;

/// A Place-tool drag in progress.
#[derive(Clone, Debug)]
pub(crate) struct PlaceDrag {
    pub(crate) entity: Entity,
    pub(crate) start_xf: Transform,
    pub(crate) cursor_start: Vec2,
    /// Everything that moves with the drag — never a surface to land on.
    pub(crate) skip: HashSet<Entity>,
    pub(crate) moved: bool,
}

/// A surface point under the cursor.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Surface {
    /// World position.
    pub(crate) pos: DVec3,
    /// Unit normal, world orientation.
    pub(crate) normal: Vec3,
}

/// Where `start` goes so that geometry whose world-space corners are
/// `corners` (placed by `start`) rests on `surface`. With `align`, the node
/// also turns so its up axis follows the surface normal.
pub(crate) fn rest_on(start: &Transform, corners: &[DVec3], surface: Surface, align: bool) -> Transform {
    let n = surface.normal.normalize_or_zero();
    let n = if n == Vec3::ZERO { Vec3::Y } else { n };
    let rotation = if align {
        (Quat::from_rotation_arc((start.rotation * Vec3::Y).normalize(), n) * start.rotation).normalize()
    } else {
        start.rotation
    };
    let turn = rotation * start.rotation.inverse();
    // How far the geometry reaches below the pivot, measured along the normal.
    let below = corners
        .iter()
        .map(|c| -(turn * (*c - start.translation).as_vec3()).dot(n))
        .fold(0.0f32, f32::max);
    Transform { translation: surface.pos + (n * below).as_dvec3(), rotation, ..*start }
}

/// Snap a resting position to the grid along the surface only: the axes the
/// normal mostly lies across. Snapping the normal's own axis would lift the
/// node off the surface or sink it in.
pub(crate) fn snap_along_surface(p: DVec3, normal: Vec3, step: f64) -> DVec3 {
    let snap = |v: f64, across: f32| if across.abs() < 0.7 { (v / step).round() * step } else { v };
    DVec3::new(snap(p.x, normal.x), snap(p.y, normal.y), snap(p.z, normal.z))
}

impl Editor {
    /// `roots` and everything parented under them.
    pub(crate) fn subtree_of(&self, roots: &[Entity]) -> HashSet<Entity> {
        let mut out: HashSet<Entity> = roots.iter().copied().collect();
        for (e, _) in self.world.query::<Transform>() {
            if roots.iter().any(|&r| self.is_descendant(e, r)) {
                out.insert(e);
            }
        }
        out
    }

    /// A node's own geometry bounds, in its local frame. `None` for a node
    /// that draws no triangles of its own.
    pub(crate) fn node_local_bounds(&mut self, e: Entity) -> Option<(Vec3, Vec3)> {
        match self.world.get::<Matter>(e)? {
            // Read from the store rather than the triangle cache, which keeps
            // the geometry it was first built from while the mesh is edited.
            Matter::MapMesh { id } => {
                let mesh = self.maps.meshes.get(id)?;
                let mut it = mesh.verts.iter();
                let first = *it.next()?;
                Some(it.fold((first, first), |(lo, hi), v| (lo.min(*v), hi.max(*v))))
            }
            Matter::Mesh { .. } | Matter::Primitive { .. } => {
                let key = self.ensure_paint_mesh_pub(e)?;
                self.paint_meshes.bounds(&key)
            }
            _ => None,
        }
    }

    /// The world-space corners of every piece of geometry in `nodes`.
    pub(crate) fn world_corners(&mut self, nodes: &HashSet<Entity>) -> Vec<DVec3> {
        let mut out = Vec::new();
        for &e in nodes {
            let Some((lo, hi)) = self.node_local_bounds(e) else { continue };
            let m = floptle_core::world_transform(&self.world, e).world_matrix();
            for i in 0..8 {
                let c = Vec3::new(
                    if i & 1 == 0 { lo.x } else { hi.x },
                    if i & 2 == 0 { lo.y } else { hi.y },
                    if i & 4 == 0 { lo.z } else { hi.z },
                );
                out.push(m.transform_point3(c.as_dvec3()));
            }
        }
        out
    }

    /// The surface a camera-relative ray lands on: the nearest node (except
    /// `skip`) or terrain, else the ground plane at y = 0 when the ray points
    /// down at it.
    pub(crate) fn surface_under(&mut self, ro: Vec3, rd: Vec3, skip: &HashSet<Entity>) -> Option<Surface> {
        let cam = self.camera.render_camera().world_position;
        let node = self.raycast_nodes(ro, rd, |e| !skip.contains(&e)).map(|h| (h.t, h.pos, h.normal));
        let terrain = self.raycast_terrains(ro, rd, skip);
        let nearest = match (node, terrain) {
            (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
            (a, b) => a.or(b),
        };
        if let Some((_, pos, normal)) = nearest {
            return Some(Surface { pos: cam + pos.as_dvec3(), normal });
        }
        let ro_w = cam + ro.as_dvec3();
        if rd.y < -1e-4 {
            let t = -ro_w.y / rd.y as f64;
            if t > 0.0 && t < 1e5 {
                return Some(Surface { pos: ro_w + rd.as_dvec3() * t, normal: Vec3::Y });
            }
        }
        None
    }

    /// The nearest terrain surface a camera-relative ray strikes:
    /// `(distance, camera-relative point, normal)`.
    fn raycast_terrains(&self, ro: Vec3, rd: Vec3, skip: &HashSet<Entity>) -> Option<(f32, Vec3, Vec3)> {
        let cam = self.camera.render_camera().world_position;
        let mut best: Option<(f32, Vec3, Vec3)> = None;
        for (&e, terrain) in &self.terrains {
            if skip.contains(&e) {
                continue;
            }
            let (origin, rot, s) = self.terrain_world_frame_of(e);
            let inv = rot.inverse();
            let ro_l = (inv * (cam + ro.as_dvec3() - origin).as_vec3()) / s;
            let rd_l = (inv * rd).normalize_or_zero();
            let Some(hit) = terrain.field.raycast(ro_l, rd_l, 4096.0 / s) else { continue };
            let hit_w = origin + (rot * (hit * s)).as_dvec3();
            let pos = (hit_w - cam).as_vec3();
            let t = (pos - ro).length();
            if best.is_none_or(|b| t < b.0) {
                let normal = (rot * terrain.field.grad(hit)).normalize_or_zero();
                best = Some((t, pos, normal));
            }
        }
        best
    }

    /// The surface under the viewport cursor, looking through `skip`.
    pub(crate) fn surface_under_cursor(&mut self, skip: &HashSet<Entity>) -> Option<Surface> {
        let (ro, rd) = self.cursor_ray(self.cursor?)?;
        self.surface_under(ro, rd, skip)
    }

    /// Set `roots` down on the surface under the cursor, keeping their
    /// arrangement relative to the first. What an asset drop does once the
    /// new node exists.
    pub(crate) fn rest_under_cursor(&mut self, roots: &[Entity]) {
        let Some(&first) = roots.first() else { return };
        let moving = self.subtree_of(roots);
        let Some(surface) = self.surface_under_cursor(&moving) else { return };
        let start = floptle_core::world_transform(&self.world, first);
        let corners = self.world_corners(&moving);
        let mut xf = rest_on(&start, &corners, surface, false);
        if self.grid.snap {
            xf.translation = snap_along_surface(xf.translation, surface.normal, self.grid.size as f64);
        }
        let delta = xf.translation - start.translation;
        for &r in roots {
            let mut t = floptle_core::world_transform(&self.world, r);
            t.translation += delta;
            self.set_world_transform(r, t);
        }
    }

    /// Draw where a dropped asset would land: a ring lying on the surface under
    /// the cursor, and a short tick along its normal.
    #[cfg(feature = "editor-ui")]
    pub(crate) fn paint_landing_marker(&mut self, ctx: &egui::Context) {
        let Some(surface) = self.surface_under_cursor(&HashSet::new()) else { return };
        let Some(gpu) = self.gpu.as_ref() else { return };
        let (w, h) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
        let cam = self.camera.render_camera();
        let vp = cam.view_proj(w / h);
        let ppp = ctx.pixels_per_point();
        let to_screen = |p: DVec3| {
            crate::viz::project(p, cam.world_position, vp, w, h).map(|s| egui::pos2(s.x / ppp, s.y / ppp))
        };
        let n = surface.normal;
        let t1 = n.cross(if n.y.abs() > 0.9 { Vec3::X } else { Vec3::Y }).normalize_or_zero();
        let t2 = n.cross(t1);
        let ring: Vec<egui::Pos2> = (0..=32)
            .filter_map(|i| {
                let a = i as f32 / 32.0 * std::f32::consts::TAU;
                to_screen(surface.pos + ((t1 * a.cos() + t2 * a.sin()) * 0.5).as_dvec3())
            })
            .collect();
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("landing")));
        let col = egui::Color32::from_rgb(235, 225, 150);
        painter.add(egui::Shape::line(ring, egui::Stroke::new(2.0, col)));
        if let (Some(a), Some(b)) = (to_screen(surface.pos), to_screen(surface.pos + (n * 0.6).as_dvec3())) {
            painter.line_segment([a, b], egui::Stroke::new(2.0, col));
            painter.circle_filled(a, 3.0, col);
        }
    }

    /// A left press with the Place tool: select what is under the cursor and
    /// pick it up. Returns false when nothing is there (the caller clears).
    pub(crate) fn place_press(&mut self, cursor: Vec2) -> bool {
        let Some(e) = self.pick(cursor) else { return false };
        if self.shift || self.ctrl {
            self.select_toggle(e);
            return true;
        }
        if !self.selection.contains(&e) {
            self.select_single(e);
        } else {
            // Grab the node that was clicked, carrying the rest of the selection.
            self.selection.retain(|&o| o != e);
            self.selection.push(e);
        }
        let roots: Vec<Entity> = self
            .selection
            .iter()
            .copied()
            .filter(|&o| !self.selection.iter().any(|&a| a != o && self.is_descendant(o, a)))
            .collect();
        self.drag_group = roots
            .iter()
            .copied()
            .filter(|&o| o != e)
            .map(|o| (o, floptle_core::world_transform(&self.world, o)))
            .collect();
        self.place_drag = Some(PlaceDrag {
            entity: e,
            start_xf: floptle_core::world_transform(&self.world, e),
            cursor_start: cursor,
            skip: self.subtree_of(&roots),
            moved: false,
        });
        true
    }

    /// Follow the cursor with a Place drag.
    pub(crate) fn place_drag_update(&mut self) {
        let (Some(mut drag), Some(cursor)) = (self.place_drag.take(), self.cursor) else { return };
        if !drag.moved && (cursor - drag.cursor_start).length() < PLACE_DRAG_PX {
            self.place_drag = Some(drag);
            return;
        }
        if !drag.moved {
            drag.moved = true;
            self.begin_edit();
        }
        if let Some(surface) = self.surface_under_cursor(&drag.skip) {
            // The corners as they were at the press, so the rest distance
            // never depends on where the drag has already put things.
            let now = floptle_core::world_transform(&self.world, drag.entity);
            let back = drag.start_xf.translation - now.translation;
            let corners: Vec<DVec3> =
                self.world_corners(&drag.skip).into_iter().map(|c| c + back).collect();
            let mut xf = rest_on(&drag.start_xf, &corners, surface, self.place_align);
            if self.grid.snap {
                xf.translation = snap_along_surface(xf.translation, surface.normal, self.grid.size as f64);
            }
            self.set_world_transform(drag.entity, xf);
            self.apply_group_transform(drag.start_xf, xf);
        }
        self.place_drag = Some(drag);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_cube_corners(at: DVec3) -> Vec<DVec3> {
        (0..8)
            .map(|i| {
                at + DVec3::new(
                    if i & 1 == 0 { -0.5 } else { 0.5 },
                    if i & 2 == 0 { -0.5 } else { 0.5 },
                    if i & 4 == 0 { -0.5 } else { 0.5 },
                )
            })
            .collect()
    }

    /// **A placed node rests on the surface, whichever way the surface faces.**
    /// Its deepest point along the normal touches the hit point — the pivot is
    /// never what lands, or every model would sink to its middle.
    #[test]
    fn a_node_rests_on_floors_walls_and_ceilings() {
        let start = Transform::from_translation(DVec3::new(3.0, 7.0, -2.0));
        let corners = unit_cube_corners(start.translation);
        let at = DVec3::new(10.0, 1.0, 5.0);
        for (normal, expect) in [
            (Vec3::Y, at + DVec3::new(0.0, 0.5, 0.0)),
            (Vec3::X, at + DVec3::new(0.5, 0.0, 0.0)),
            (Vec3::NEG_Y, at - DVec3::new(0.0, 0.5, 0.0)),
        ] {
            let xf = rest_on(&start, &corners, Surface { pos: at, normal }, false);
            assert!((xf.translation - expect).length() < 1e-5, "{normal}: {}", xf.translation);
            assert_eq!(xf.rotation, start.rotation, "placement keeps the node's turn");
        }

        // A model whose pivot is at its feet already rests with no lift.
        let feet: Vec<DVec3> = corners.iter().map(|c| *c + DVec3::new(0.0, 0.5, 0.0)).collect();
        let xf = rest_on(&start, &feet, Surface { pos: at, normal: Vec3::Y }, false);
        assert!((xf.translation - at).length() < 1e-5);

        // Aligned to a wall, the node's up follows the wall's normal.
        let xf = rest_on(&start, &corners, Surface { pos: at, normal: Vec3::X }, true);
        assert!((xf.rotation * Vec3::Y - Vec3::X).length() < 1e-5);
        assert!((xf.translation - (at + DVec3::new(0.5, 0.0, 0.0))).length() < 1e-5);
    }

    #[test]
    fn grid_snap_slides_along_the_surface_only() {
        let p = DVec3::new(1.3, 0.37, -2.6);
        assert_eq!(snap_along_surface(p, Vec3::Y, 1.0), DVec3::new(1.0, 0.37, -3.0));
        assert_eq!(snap_along_surface(p, Vec3::X, 0.5), DVec3::new(1.3, 0.5, -2.5));
    }
}
