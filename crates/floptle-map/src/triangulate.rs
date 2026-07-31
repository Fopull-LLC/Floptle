//! MapMesh -> render geometry: one flat-shaded mesh per material slot.

use crate::{Face, MapMesh};
use glam::Vec3;
use std::collections::BTreeMap;

/// Triangulated render geometry for one material slot. The editor turns this
/// into a dynamic GPU mesh part; `tri_faces` maps each output triangle back to
/// its source face index for picking.
#[derive(Clone, Debug, Default)]
pub struct SlotMesh {
    pub slot: u16,
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
    /// Per-TRIANGLE (indices.len()/3 entries) source face index in the MapMesh.
    pub tri_faces: Vec<u32>,
    /// Per-VERTEX source `(face index, MapMesh vertex index)`. Corners are not
    /// shared between faces, so this is what lets paint follow the surface
    /// across a re-triangulation: the editor turns it into a durable key and
    /// carries the colours over (see `map_paint`).
    pub vert_src: Vec<(u32, u32)>,
}

/// Face normal via Newell's method (robust for near-planar n-gons), normalized.
/// Degenerate faces (zero area) return `Vec3::Y` rather than NaN.
pub fn face_normal(mesh: &MapMesh, face: &Face) -> Vec3 {
    newell(mesh, face).try_normalize().unwrap_or(Vec3::Y)
}

/// Unnormalized Newell vector — magnitude is 2x the polygon area, so it doubles
/// as the area weight for region-extrude's average normal.
pub(crate) fn newell(mesh: &MapMesh, face: &Face) -> Vec3 {
    let mut n = Vec3::ZERO;
    let k = face.verts.len();
    for i in 0..k {
        let (ai, bi) = (face.verts[i] as usize, face.verts[(i + 1) % k] as usize);
        if ai >= mesh.verts.len() || bi >= mesh.verts.len() {
            continue;
        }
        let (a, b) = (mesh.verts[ai], mesh.verts[bi]);
        n += (a - b).cross(a + b);
    }
    n
}

/// Dominant-axis planar projection: 1 local unit = 1 UV tile, axis pairs chosen
/// for upright, unmirrored walls and floors (see `triangulate` docs).
fn face_uv(p: Vec3, n: Vec3) -> [f32; 2] {
    let sgn = |v: f32| if v < 0.0 { -1.0 } else { 1.0 };
    let (ax, ay, az) = (n.x.abs(), n.y.abs(), n.z.abs());
    if ax >= ay && ax >= az {
        [p.z * -sgn(n.x), -p.y]
    } else if ay >= az {
        [p.x, p.z * sgn(n.y)]
    } else {
        [p.x * sgn(n.z), -p.y]
    }
}

/// Triangulate every face into per-slot flat-shaded meshes.
///
/// Semantics (the editor and tests rely on these exactly):
/// - One `SlotMesh` per slot index that has at least one face, ordered by
///   slot index ascending. Out-of-range face slots clamp to 0.
/// - Vertices are NOT shared between faces (flat shading: every corner gets
///   the face normal). Within one face, corners are emitted once and the face
///   is fan-triangulated: `(0, i, i+1)` over the face's corner order, which
///   preserves CCW winding.
/// - `uvs` are a dominant-axis planar projection of the local position: pick
///   the normal's largest |component| axis, project onto the other two axes,
///   1 unit = 1 UV tile. Exact axis pairs (box mapping — upright, unmirrored
///   walls and floors): X-dominant -> (u, v) = (p.z * -sign(n.x), -p.y);
///   Y-dominant -> (p.x, p.z * sign(n.y)); Z-dominant ->
///   (p.x * sign(n.z), -p.y). `sign(0)` counts as +1.
/// - `tri_faces[t]` = MapMesh face index of triangle `t`.
pub fn triangulate(mesh: &MapMesh) -> Vec<SlotMesh> {
    let nslots = mesh.slots.len().max(1) as u16;
    let mut by_slot: BTreeMap<u16, Vec<usize>> = BTreeMap::new();
    for (fi, f) in mesh.faces.iter().enumerate() {
        // Defensive: a hand-edited sidecar must render, not crash.
        if f.verts.len() < 3 || f.verts.iter().any(|&v| v as usize >= mesh.verts.len()) {
            continue;
        }
        let slot = if f.slot < nslots { f.slot } else { 0 };
        by_slot.entry(slot).or_default().push(fi);
    }
    let mut out = Vec::new();
    for (slot, faces) in by_slot {
        let mut sm = SlotMesh { slot, ..Default::default() };
        for fi in faces {
            let f = &mesh.faces[fi];
            let n = face_normal(mesh, f);
            let base = sm.positions.len() as u32;
            for &vi in &f.verts {
                let p = mesh.verts[vi as usize];
                sm.positions.push(p.to_array());
                sm.normals.push(n.to_array());
                sm.uvs.push(face_uv(p, n));
                sm.vert_src.push((fi as u32, vi));
            }
            for i in 1..f.verts.len() as u32 - 1 {
                sm.indices.extend_from_slice(&[base, base + i, base + i + 1]);
                sm.tri_faces.push(fi as u32);
            }
        }
        if !sm.indices.is_empty() {
            out.push(sm);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{box_mesh, set_face_slot};
    use glam::Vec3;

    #[test]
    fn box_triangulation_invariants() {
        let m = box_mesh(Vec3::ONE);
        let slots = triangulate(&m);
        assert_eq!(slots.len(), 1);
        let s = &slots[0];
        assert_eq!(s.slot, 0);
        assert_eq!(s.positions.len(), 24); // 6 quads, corners unshared
        assert_eq!(s.indices.len(), 36);
        assert_eq!(s.tri_faces.len(), 12);
        assert_eq!(s.normals.len(), 24);
        assert_eq!(s.uvs.len(), 24);
        // Every emitted corner says which face and which source vertex it came
        // from, in step with the positions.
        assert_eq!(s.vert_src.len(), 24);
        for (i, &(fi, vi)) in s.vert_src.iter().enumerate() {
            assert!(m.faces[fi as usize].verts.contains(&vi));
            assert_eq!(Vec3::from(s.positions[i]), m.verts[vi as usize]);
        }
        for &i in &s.indices {
            assert!((i as usize) < s.positions.len());
        }
        // Flat normals: unit length, equal to the source face normal, and the
        // emitted triangle winding agrees with it (CCW preserved).
        for (t, &fi) in s.tri_faces.iter().enumerate() {
            let fnorm = face_normal(&m, &m.faces[fi as usize]);
            let [i0, i1, i2] =
                [s.indices[t * 3] as usize, s.indices[t * 3 + 1] as usize, s.indices[t * 3 + 2] as usize];
            for i in [i0, i1, i2] {
                let n = Vec3::from(s.normals[i]);
                assert!((n.length() - 1.0).abs() < 1e-4);
                assert!(n.distance(fnorm) < 1e-4);
            }
            let (a, b, c) =
                (Vec3::from(s.positions[i0]), Vec3::from(s.positions[i1]), Vec3::from(s.positions[i2]));
            let tri_n = (b - a).cross(c - a);
            assert!(tri_n.dot(fnorm) > 0.0, "tri {t} winds against its face normal");
        }
    }

    #[test]
    fn slots_group_and_order() {
        let mut m = box_mesh(Vec3::ONE);
        m.slots.push("Wall".into());
        set_face_slot(&mut m, &[0, 2], 1);
        let slots = triangulate(&m);
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].slot, 0);
        assert_eq!(slots[1].slot, 1);
        assert_eq!(slots[0].tri_faces.len(), 8);
        assert_eq!(slots[1].tri_faces.len(), 4);
        assert!(slots[1].tri_faces.iter().all(|&f| f == 0 || f == 2));
    }

    #[test]
    fn out_of_range_slot_clamps_to_zero() {
        let mut m = box_mesh(Vec3::ONE);
        m.faces[3].slot = 99;
        let slots = triangulate(&m);
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].slot, 0);
        assert_eq!(slots[0].tri_faces.len(), 12);
    }

    #[test]
    fn uv_rule_on_a_unit_box() {
        // half = 1 box: +X face has p.x = 1; UV = (p.z * -1, -p.y).
        let m = box_mesh(Vec3::ONE);
        let slots = triangulate(&m);
        let s = &slots[0];
        for i in 0..s.positions.len() {
            let p = Vec3::from(s.positions[i]);
            let n = Vec3::from(s.normals[i]);
            let uv = s.uvs[i];
            if n.x > 0.9 {
                assert_eq!(uv, [-p.z, -p.y]);
            } else if n.x < -0.9 {
                assert_eq!(uv, [p.z, -p.y]);
            } else if n.y > 0.9 {
                assert_eq!(uv, [p.x, p.z]);
            } else if n.z < -0.9 {
                assert_eq!(uv, [-p.x, -p.y]);
            }
        }
    }

    #[test]
    fn hostile_faces_are_skipped_not_fatal() {
        let mut m = box_mesh(Vec3::ONE);
        m.faces.push(crate::Face { verts: vec![0, 99, 2, 3], slot: 0 }); // out of range
        m.faces.push(crate::Face { verts: vec![0, 1], slot: 0 }); // degenerate
        let slots = triangulate(&m);
        assert_eq!(slots[0].tri_faces.len(), 12); // just the box
    }
}
