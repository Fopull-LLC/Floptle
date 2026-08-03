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

/// Split one face into triangles, as local corner indices (0..k) into `face.verts`.
///
/// **Not a fan from corner 0**, which is what this used to be and what made editing
/// feel broken. A fan is only correct for a face that is both convex and planar, and
/// dragging a single vertex destroys either property:
///
/// - **Concave** — a fan emits triangles that leave the polygon entirely, so the face
///   visibly spills outside its own outline.
/// - **Non-planar** — every triangle shares corner 0, so the whole face creases around
///   that one corner. Which corner is "first" is an artifact of how the face happened to
///   be built, so the same drag on two faces of the same shape folds them differently,
///   and dragging corner 0 itself deforms the face in a way dragging any other corner
///   does not. That asymmetry is the "folding/glitching" — it is not a rendering
///   artifact, the triangles really are those shapes.
///
/// So: a quad takes its **shorter diagonal** (the standard choice — it minimizes the
/// crease on a warped quad and is symmetric under corner rotation), and anything larger
/// is **ear-clipped** in the face's own plane, which handles concave outlines correctly.
/// Ear clipping can fail on a self-intersecting projection; that falls back to the fan,
/// because a wrong-looking face beats a missing one.
fn face_tris(mesh: &MapMesh, face: &Face, n: Vec3) -> Vec<[u32; 3]> {
    let k = face.verts.len();
    let p = |i: usize| mesh.verts[face.verts[i] as usize];
    if k == 3 {
        return vec![[0, 1, 2]];
    }
    if k == 4 {
        // Shorter diagonal. Equal lengths keep 0–2 so a planar quad is unchanged.
        return if p(0).distance_squared(p(2)) <= p(1).distance_squared(p(3)) {
            vec![[0, 1, 2], [0, 2, 3]]
        } else {
            vec![[1, 2, 3], [1, 3, 0]]
        };
    }

    // Project onto the face plane. `u`/`v` are right-handed about `n`, so a CCW
    // outline in 3D stays CCW in 2D and the winding checks below mean what they say.
    let u = if n.x.abs() < 0.9 { Vec3::X.cross(n) } else { Vec3::Y.cross(n) }
        .try_normalize()
        .unwrap_or(Vec3::X);
    let v = n.cross(u);
    let o = p(0);
    let pts: Vec<[f32; 2]> =
        (0..k).map(|i| [(p(i) - o).dot(u), (p(i) - o).dot(v)]).collect();
    let cross = |a: [f32; 2], b: [f32; 2], c: [f32; 2]| {
        (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
    };
    // ON an edge counts as inside. A strict test looks more permissive and is wrong:
    // the reflex corner of an L sits exactly on the diagonal of the ear you would cut
    // across the notch, so a strict test calls it "outside", accepts the ear, and lays
    // a triangle over the hole — the very artifact this function exists to stop.
    let inside = |a: [f32; 2], b: [f32; 2], c: [f32; 2], q: [f32; 2]| {
        cross(a, b, q) >= 0.0 && cross(b, c, q) >= 0.0 && cross(c, a, q) >= 0.0
    };

    let mut idx: Vec<u32> = (0..k as u32).collect();
    let mut out = Vec::with_capacity(k - 2);
    let mut guard = 0;
    while idx.len() > 3 {
        guard += 1;
        if guard > k * k {
            out.clear();
            break; // self-intersecting or degenerate — fall back below
        }
        let m = idx.len();
        // Only REFLEX corners can invalidate an ear, and only reflex corners are worth
        // testing with the inclusive rule above — testing every corner that way would
        // reject ears whose neighbours merely touch them, which is most of them.
        let reflex: Vec<bool> = (0..m)
            .map(|j| {
                let (a, b, c) = (
                    pts[idx[(j + m - 1) % m] as usize],
                    pts[idx[j] as usize],
                    pts[idx[(j + 1) % m] as usize],
                );
                cross(a, b, c) <= 0.0
            })
            .collect();
        let mut clipped = false;
        for i in 0..m {
            let (ia, ib, ic) = (idx[(i + m - 1) % m], idx[i], idx[(i + 1) % m]);
            let (a, b, c) = (pts[ia as usize], pts[ib as usize], pts[ic as usize]);
            if cross(a, b, c) <= 0.0 {
                continue; // reflex corner (or collinear) — not an ear
            }
            let blocked = (0..m).any(|j| {
                let v = idx[j];
                reflex[j] && v != ia && v != ib && v != ic && inside(a, b, c, pts[v as usize])
            });
            if blocked {
                continue;
            }
            out.push([ia, ib, ic]);
            idx.remove(i);
            clipped = true;
            break;
        }
        if !clipped {
            out.clear();
            break;
        }
    }
    if out.is_empty() {
        return (1..k as u32 - 1).map(|i| [0, i, i + 1]).collect();
    }
    out.push([idx[0], idx[1], idx[2]]);
    out
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
///   the face normal). Within one face, corners are emitted once and the face is
///   split by [`face_tris`] — shorter diagonal for a quad, ear clipping above that
///   — which preserves CCW winding and does not fold on a concave or warped face.
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
            for [a, b, c] in face_tris(mesh, f, n) {
                sm.indices.extend_from_slice(&[base + a, base + b, base + c]);
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

    /// Total triangle area must equal the polygon's own area. A fan from corner 0 on a
    /// CONCAVE face emits triangles that stick out past the outline, so the total comes
    /// out larger — that overspill is what "the face folds over itself" looks like.
    #[test]
    fn a_concave_face_does_not_spill_outside_its_outline() {
        // An L, CCW in the XZ plane (normal +Y). Corner 2 is reflex.
        let m = MapMesh {
            verts: vec![
                Vec3::new(0.0, 0.0, 0.0),
                Vec3::new(2.0, 0.0, 0.0),
                Vec3::new(2.0, 0.0, 1.0),
                Vec3::new(1.0, 0.0, 1.0),
                Vec3::new(1.0, 0.0, 2.0),
                Vec3::new(0.0, 0.0, 2.0),
            ],
            faces: vec![Face { verts: vec![0, 5, 4, 3, 2, 1], slot: 0 }],
            slots: vec!["Default".into()],
            spec: None,
        };
        let s = &triangulate(&m)[0];
        assert_eq!(s.tri_faces.len(), 4, "an L is 4 triangles");
        let mut area = 0.0f32;
        for t in 0..s.tri_faces.len() {
            let [i0, i1, i2] = [
                s.indices[t * 3] as usize,
                s.indices[t * 3 + 1] as usize,
                s.indices[t * 3 + 2] as usize,
            ];
            let (a, b, c) = (
                Vec3::from(s.positions[i0]),
                Vec3::from(s.positions[i1]),
                Vec3::from(s.positions[i2]),
            );
            area += (b - a).cross(c - a).length() * 0.5;
        }
        // The L covers 3 unit squares. The old fan gave 4 — a whole extra square of
        // geometry laid over the notch.
        assert!((area - 3.0).abs() < 1e-4, "covered {area}, want 3.0");

        // …and every triangle still winds with the face.
        let fnorm = face_normal(&m, &m.faces[0]);
        for t in 0..s.tri_faces.len() {
            let [i0, i1, i2] = [
                s.indices[t * 3] as usize,
                s.indices[t * 3 + 1] as usize,
                s.indices[t * 3 + 2] as usize,
            ];
            let (a, b, c) = (
                Vec3::from(s.positions[i0]),
                Vec3::from(s.positions[i1]),
                Vec3::from(s.positions[i2]),
            );
            assert!((b - a).cross(c - a).dot(fnorm) > 0.0, "tri {t} winds backwards");
        }
    }

    /// A warped quad must split the same way regardless of which corner is listed
    /// first. The fan could not: it always creased around corner 0, so rotating the
    /// corner order — or dragging corner 0 rather than corner 2 — changed the shape.
    #[test]
    fn a_warped_quad_splits_the_same_way_whichever_corner_is_first() {
        let corners = [
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(3.0, 0.0, 0.0),
            Vec3::new(3.0, 1.0, 1.0), // lifted: the quad is no longer planar
            Vec3::new(0.0, 0.0, 1.0),
        ];
        // The diagonal actually chosen, as a pair of world positions.
        let diagonal_of = |rot: usize| {
            let verts: Vec<Vec3> = (0..4).map(|i| corners[(i + rot) % 4]).collect();
            let m = MapMesh {
                verts,
                faces: vec![Face { verts: vec![0, 1, 2, 3], slot: 0 }],
                slots: vec!["Default".into()],
                spec: None,
            };
            let s = &triangulate(&m)[0];
            assert_eq!(s.tri_faces.len(), 2);
            // The shared edge of the two triangles IS the diagonal.
            let tri: Vec<Vec<usize>> = (0..2)
                .map(|t| (0..3).map(|k| s.indices[t * 3 + k] as usize).collect())
                .collect();
            let mut shared: Vec<Vec3> = tri[0]
                .iter()
                .filter(|i| tri[1].contains(i))
                .map(|&i| Vec3::from(s.positions[i]))
                .collect();
            shared.sort_by(|a, b| a.to_array().partial_cmp(&b.to_array()).unwrap());
            shared
        };
        let base = diagonal_of(0);
        assert_eq!(base.len(), 2, "the two triangles share exactly one edge");
        for rot in 1..4 {
            assert_eq!(diagonal_of(rot), base, "corner order {rot} chose a different diagonal");
        }
        // And it is genuinely the SHORTER one: 0–2 spans the lift, 1–3 does not.
        assert!(
            base[0].distance(base[1]) < corners[0].distance(corners[2]) - 1e-4,
            "picked the long diagonal"
        );
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
