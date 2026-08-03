//! floptle-map — the editable polygon-mesh kernel behind the map-building suite.
//!
//! A [`MapMesh`] is authoring-side geometry: n-gon faces over a shared vertex
//! pool, each face tagged with a material *slot*. The editor renders it by
//! [`triangulate`]-ing into one flat-shaded render mesh per slot (so per-face
//! materials ride the engine's existing per-part material path), and edits it
//! through the ops in this crate. Everything here is pure geometry — no GPU,
//! no ECS — so every op is unit-testable in isolation.
//!
//! Conventions (load-bearing, the editor relies on all of these):
//! - Face winding is **CCW viewed from outside**; the face normal follows the
//!   right-hand rule (Newell's method for n-gons — see [`face_normal`]).
//! - Faces are assumed **convex and near-planar**; triangulation is a fan from
//!   the face's first vertex. Ops in this crate only produce convex faces.
//! - Positions are **object-local**; the scene node's Transform places them.
//! - UVs are a dominant-axis planar projection of the local position, 1 unit =
//!   1 UV tile, so textures tile consistently across a blockout with zero
//!   unwrapping (material tiling settings scale from there).
//! - `slots` is never empty; new meshes start with one slot named `"Default"`.
//!   Face `slot` indices out of range are treated as 0 (defensive: a hand-
//!   edited sidecar must never crash the editor).

use glam::{Mat4, Vec2, Vec3};
use serde::{Deserialize, Serialize};

mod knife;
mod ops;
mod primitives;
mod raycast;
mod select;
mod triangulate;

pub use knife::{face_plane_hit, knife, knife_refusal, nearest_cut_point, CutPoint, KnifeCut};
pub use ops::{
    bridge_faces, delete_faces, detach_faces, extrude_faces, flip_faces, inset_faces, merge_into,
    recenter, recenter_on, resize, set_face_slot, snap_verts, subdivide_faces, transform_verts,
    translate_verts, weld,
};
pub use primitives::{arch, box_mesh, cylinder, plane, sphere, stairs, wedge, ShapeKind, ShapeSpec};
pub use raycast::{raycast, FaceHit};
pub use select::{
    connected_faces, coplanar_faces, edge_loop, faces_with_slot, front_facing, grow_faces,
    non_planar_faces, shrink_faces,
};
pub use triangulate::{face_normal, triangulate, SlotMesh};

/// One n-gon face: indices into [`MapMesh::verts`], CCW from outside, plus the
/// material slot it draws with.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Face {
    pub verts: Vec<u32>,
    #[serde(default)]
    pub slot: u16,
}

/// Editable polygon mesh: shared vertex pool + n-gon faces + material slots.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MapMesh {
    pub verts: Vec<Vec3>,
    pub faces: Vec<Face>,
    /// Material slot names (never empty). A face's `slot` indexes this; the
    /// editor keys per-node material overrides by these names.
    pub slots: Vec<String>,
    /// The generator that produced this mesh, while it is still untouched —
    /// the editor uses it to re-generate with different parameters (stair
    /// steps, cylinder sides). Cleared by the first op that moves geometry.
    /// `#[serde(default)]` so sidecars written before this existed still load.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<ShapeSpec>,
}

impl MapMesh {
    /// Empty mesh with the default slot — the invariant-preserving constructor.
    pub fn new() -> Self {
        Self { verts: Vec::new(), faces: Vec::new(), slots: vec!["Default".into()], spec: None }
    }

    /// Object-local AABB; `None` when the mesh has no vertices.
    pub fn bounds(&self) -> Option<(Vec3, Vec3)> {
        let mut it = self.verts.iter();
        let first = *it.next()?;
        let (mut lo, mut hi) = (first, first);
        for &v in it {
            lo = lo.min(v);
            hi = hi.max(v);
        }
        Some((lo, hi))
    }

    /// Unique undirected edges, each as `(a, b)` with `a < b`, in first-seen
    /// order (stable across calls on an unchanged mesh — the editor indexes
    /// edge selections into this list).
    pub fn edges(&self) -> Vec<(u32, u32)> {
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for f in &self.faces {
            let n = f.verts.len();
            for i in 0..n {
                let (a, b) = (f.verts[i], f.verts[(i + 1) % n]);
                let key = (a.min(b), a.max(b));
                if seen.insert(key) {
                    out.push(key);
                }
            }
        }
        out
    }

    /// Structural sanity: every face index in range, faces have >= 3 verts,
    /// slots non-empty. Ops must leave any valid mesh valid.
    pub fn validate(&self) -> Result<(), String> {
        if self.slots.is_empty() {
            return Err("slots is empty".into());
        }
        for (fi, f) in self.faces.iter().enumerate() {
            if f.verts.len() < 3 {
                return Err(format!("face {fi} has {} verts", f.verts.len()));
            }
            for &v in &f.verts {
                if v as usize >= self.verts.len() {
                    return Err(format!("face {fi} references vert {v} out of range"));
                }
            }
        }
        Ok(())
    }
}

impl Default for MapMesh {
    fn default() -> Self {
        Self::new()
    }
}

// Re-exported math aliases so editor code doesn't need to spell glam paths.
pub type V2 = Vec2;
pub type V3 = Vec3;
pub type M4 = Mat4;
