//! The knife: divide a face in two along a segment drawn between two points on
//! its border.
//!
//! Two rules make the result a mesh rather than a picture of one:
//!
//! 1. **The cut runs border to border.** A face is a closed ring of corners; a
//!    segment that starts or ends in its interior can't split it into two rings.
//!    So a cut point is always either an existing corner or a point along one of
//!    the face's edges ([`CutPoint`]), and the editor snaps the cursor onto the
//!    nearest one.
//! 2. **Splitting an edge splits it for EVERY face that uses it.** Cutting a
//!    wall's edge without telling the floor that shares it leaves a T-junction:
//!    the floor still spans the full edge while the wall now has a corner half
//!    way along it, and the rasteriser shows a hairline crack down the seam. So
//!    [`knife`] inserts the new corner into every face on that edge, and the
//!    neighbour stays welded — it just gains a redundant (collinear) corner.

use crate::{Face, MapMesh};
use glam::Vec3;

/// Where a knife cut starts or ends: an existing corner of the face, or a point
/// along one of its edges.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CutPoint {
    Vert(u32),
    /// A point `t` of the way from `a` to `b` (`t` in `0..=1`).
    Edge { a: u32, b: u32, t: f32 },
}

/// How close to an edge's end counts as "that corner". In UV-free object units
/// this is a fraction of the edge, so it scales with the edge rather than with
/// the level.
const END_SNAP: f32 = 1e-3;

impl CutPoint {
    /// Object-space position, or `None` when it names geometry the mesh hasn't
    /// got (a stale selection outliving an undo).
    pub fn position(self, mesh: &MapMesh) -> Option<Vec3> {
        match self {
            CutPoint::Vert(v) => mesh.verts.get(v as usize).copied(),
            CutPoint::Edge { a, b, t } => {
                let (pa, pb) = (*mesh.verts.get(a as usize)?, *mesh.verts.get(b as usize)?);
                Some(pa.lerp(pb, t.clamp(0.0, 1.0)))
            }
        }
    }

    /// The canonical edge this point sits on, if any — two points on the SAME
    /// edge can never divide a face, so the editor refuses that pairing early
    /// (with a message) instead of letting the cut fail at the end.
    fn edge_key(self) -> Option<(u32, u32)> {
        match self {
            CutPoint::Vert(_) => None,
            CutPoint::Edge { a, b, t } if t > END_SNAP && t < 1.0 - END_SNAP => {
                Some((a.min(b), a.max(b)))
            }
            CutPoint::Edge { .. } => None,
        }
    }
}

/// The nearest point on `face`'s border to `p` (object space).
///
/// `vert_snap` is the distance within which an edge point becomes the corner
/// itself — without it a cut aimed at a corner lands a hair away from it and
/// leaves a sliver triangle nobody asked for.
pub fn nearest_cut_point(mesh: &MapMesh, face: u32, p: Vec3, vert_snap: f32) -> Option<CutPoint> {
    let f = mesh.faces.get(face as usize)?;
    let n = f.verts.len();
    if n < 3 {
        return None;
    }
    let mut best: Option<(f32, CutPoint)> = None;
    for i in 0..n {
        let (ai, bi) = (f.verts[i], f.verts[(i + 1) % n]);
        let (a, b) = (*mesh.verts.get(ai as usize)?, *mesh.verts.get(bi as usize)?);
        let ab = b - a;
        let len2 = ab.length_squared();
        let t = if len2 < 1e-12 { 0.0 } else { (p - a).dot(ab) / len2 };
        let t = t.clamp(0.0, 1.0);
        let at = a + ab * t;
        let d = at.distance(p);
        // Corner snap first: an aimed-at corner should win over the edge it
        // happens to be nearer along.
        let cand = if a.distance(p) <= vert_snap {
            CutPoint::Vert(ai)
        } else if b.distance(p) <= vert_snap {
            CutPoint::Vert(bi)
        } else {
            CutPoint::Edge { a: ai, b: bi, t }
        };
        let d = match cand {
            CutPoint::Vert(v) => mesh.verts[v as usize].distance(p),
            CutPoint::Edge { .. } => d,
        };
        if best.as_ref().is_none_or(|&(bd, _)| d < bd) {
            best = Some((d, cand));
        }
    }
    best.map(|(_, c)| c)
}

/// Where the ray `ro + t*rd` meets `face`'s PLANE — unbounded, so it answers
/// even when the cursor is off the face or another face is in front of it.
///
/// This is how the knife aims once a cut is under way: the face is already
/// chosen, so re-picking it from whatever the ray hits first is not a service —
/// it is how a cut across a box corner silently restarts on the neighbouring
/// face. `None` when the ray is parallel to the plane, or points away from it.
pub fn face_plane_hit(mesh: &MapMesh, face: u32, ro: Vec3, rd: Vec3) -> Option<Vec3> {
    let f = mesh.faces.get(face as usize)?;
    let n = crate::face_normal(mesh, f);
    let p0 = *mesh.verts.get(*f.verts.first()? as usize)?;
    let denom = n.dot(rd);
    if denom.abs() < 1e-9 {
        return None;
    }
    let t = n.dot(p0 - ro) / denom;
    (t >= 0.0 && t.is_finite()).then(|| ro + rd * t)
}

/// Which corner of `face`'s ring a cut point resolves to, or the edge it sits
/// along — the shape of the ring AFTER materializing it, without materializing
/// anything. `Ok(Some(i))` = ring index `i`; `Ok(None)` = mid-edge between ring
/// indices `(i, i+1)`, returned as `Err`-free via the second tuple slot.
fn ring_slot(mesh: &MapMesh, ring: &[u32], p: CutPoint) -> Option<RingSlot> {
    match p {
        CutPoint::Vert(v) => ring.iter().position(|&x| x == v).map(RingSlot::Corner),
        CutPoint::Edge { a, b, t } => {
            let t = t.clamp(0.0, 1.0);
            if t <= END_SNAP {
                return ring.iter().position(|&x| x == a).map(RingSlot::Corner);
            }
            if t >= 1.0 - END_SNAP {
                return ring.iter().position(|&x| x == b).map(RingSlot::Corner);
            }
            let _ = mesh;
            let n = ring.len();
            (0..n)
                .find(|&i| {
                    let (x, y) = (ring[i], ring[(i + 1) % n]);
                    (x == a && y == b) || (x == b && y == a)
                })
                .map(RingSlot::MidEdge)
        }
    }
}

/// Where a cut point lands on a face's ring: on corner `i`, or part way along
/// the edge that leaves corner `i`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RingSlot {
    Corner(usize),
    MidEdge(usize),
}

/// Why this cut would be refused, or `None` if it would work — **without
/// touching the mesh**.
///
/// [`knife`] runs this first, and the editor runs it every frame so the preview
/// can say "this cut will not happen" while you are still aiming, instead of
/// after you have clicked. One source of truth for the rules, so the telegraph
/// and the operation can never disagree.
pub fn knife_refusal(mesh: &MapMesh, face: u32, p0: CutPoint, p1: CutPoint) -> Option<String> {
    let f = mesh.faces.get(face as usize)?;
    if p0 == p1 {
        return Some("the cut starts and ends in the same place".into());
    }
    if let (Some(k0), Some(k1)) = (p0.edge_key(), p1.edge_key())
        && k0 == k1
    {
        return Some("both ends are on the same edge — a cut has to cross the face".into());
    }
    let ring = &f.verts;
    let n = ring.len();
    if n < 3 {
        return Some("that face has no interior to cut".into());
    }
    let (Some(s0), Some(s1)) = (ring_slot(mesh, ring, p0), ring_slot(mesh, ring, p1)) else {
        return Some("that cut doesn't run between two corners of the same face".into());
    };
    // Adjacency AFTER materializing: a mid-edge point becomes a new corner
    // between the two ends of its edge, so it is adjacent to exactly those two.
    let joined = match (s0, s1) {
        (RingSlot::Corner(i), RingSlot::Corner(j)) => {
            i == j || (i + 1) % n == j || (j + 1) % n == i
        }
        (RingSlot::Corner(i), RingSlot::MidEdge(e))
        | (RingSlot::MidEdge(e), RingSlot::Corner(i)) => i == e || i == (e + 1) % n,
        // Two mid-edge points on DIFFERENT edges are never adjacent: each one's
        // neighbours are the original corners of its own edge.
        (RingSlot::MidEdge(a), RingSlot::MidEdge(b)) => a == b,
    };
    if joined {
        return Some("those two corners are already joined by an edge".into());
    }
    None
}

/// Insert a corner `t` of the way along the undirected edge `(a, b)` into every
/// face that uses it, returning the new vertex index. This is the no-T-junction
/// half of the knife — see the module docs.
fn split_edge(mesh: &mut MapMesh, a: u32, b: u32, t: f32) -> u32 {
    let p = mesh.verts[a as usize].lerp(mesh.verts[b as usize], t);
    let new = mesh.verts.len() as u32;
    mesh.verts.push(p);
    for f in &mut mesh.faces {
        let n = f.verts.len();
        // Walk backwards so an insert can't shift a position we haven't looked
        // at yet (a face may legitimately use the edge more than once).
        for i in (0..n).rev() {
            let (x, y) = (f.verts[i], f.verts[(i + 1) % n]);
            if (x == a && y == b) || (x == b && y == a) {
                f.verts.insert(i + 1, new);
            }
        }
    }
    new
}

/// Resolve a cut point to a vertex index, creating it (and splitting the edge
/// for every face) when it lands mid-edge.
fn materialize(mesh: &mut MapMesh, p: CutPoint) -> Result<u32, String> {
    match p {
        CutPoint::Vert(v) if (v as usize) < mesh.verts.len() => Ok(v),
        CutPoint::Vert(v) => Err(format!("vertex {v} is not in this mesh")),
        CutPoint::Edge { a, b, t } => {
            if a as usize >= mesh.verts.len() || b as usize >= mesh.verts.len() {
                return Err("that edge is not in this mesh".into());
            }
            let t = t.clamp(0.0, 1.0);
            if t <= END_SNAP {
                Ok(a)
            } else if t >= 1.0 - END_SNAP {
                Ok(b)
            } else {
                Ok(split_edge(mesh, a, b, t))
            }
        }
    }
}

/// What one cut produced: the two halves, and the corners the cut ran between
/// (which is what lets the editor CHAIN — the next cut starts where this one
/// ended, so a groove can be walked across several faces).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KnifeCut {
    /// The original face index, now holding the first half.
    pub a: u32,
    /// The face index the second half was appended at.
    pub b: u32,
    pub v0: u32,
    pub v1: u32,
}

/// Cut `face` in two along the segment from `p0` to `p1`.
///
/// Returns the two halves and the corners the cut runs between. The slot is
/// inherited by both halves, and the mesh stops being parametric (the shape
/// parameters could no longer regenerate it).
///
/// Errors, rather than producing rubbish, when the two points can't divide the
/// face: the same corner twice, two points on one edge, or two corners that are
/// already neighbours (the "cut" is an edge that exists).
pub fn knife(out: &mut MapMesh, face: u32, p0: CutPoint, p1: CutPoint) -> Result<KnifeCut, String> {
    if face as usize >= out.faces.len() {
        return Err("no face to cut".into());
    }
    // Every reason a cut can be refused, decided BEFORE anything is edited and
    // by the same function the editor's live preview calls — so what the
    // telegraph shows and what the click does can never disagree.
    if let Some(why) = knife_refusal(out, face, p0, p1) {
        return Err(why);
    }
    // Everything below runs on a copy and is committed only if the whole cut
    // works: materializing the first point splits an edge for every face that
    // uses it, and a refusal after that would leave the mesh carrying half a
    // cut nobody asked for.
    let mut work = out.clone();
    let v0 = materialize(&mut work, p0)?;
    // `p1`'s edge is untouched by `p0`'s split (they're different edges, checked
    // above), so its endpoints still name the same corners.
    let v1 = materialize(&mut work, p1)?;
    if v0 == v1 {
        return Err("the cut starts and ends at the same corner".into());
    }
    let mesh = &mut work;
    let ring = mesh.faces[face as usize].verts.clone();
    let (Some(i0), Some(i1)) = (
        ring.iter().position(|&v| v == v0),
        ring.iter().position(|&v| v == v1),
    ) else {
        return Err("that cut doesn't run between two corners of the same face".into());
    };
    let (lo, hi) = (i0.min(i1), i0.max(i1));
    let n = ring.len();
    if hi - lo < 2 || (lo == 0 && hi == n - 1) {
        return Err("those two corners are already joined by an edge".into());
    }
    let a: Vec<u32> = ring[lo..=hi].to_vec();
    let b: Vec<u32> = ring[hi..].iter().chain(ring[..=lo].iter()).copied().collect();
    let slot = mesh.faces[face as usize].slot;
    mesh.faces[face as usize] = Face { verts: a, slot };
    mesh.faces.push(Face { verts: b, slot });
    mesh.spec = None; // edited geometry is no longer the primitive it was drawn as
    let new = mesh.faces.len() as u32 - 1;
    *out = work;
    Ok(KnifeCut { a: face, b: new, v0, v1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::box_mesh;

    /// The face a box's +Y quad lives at, and its four corners.
    fn top_face(m: &MapMesh) -> u32 {
        (0..m.faces.len() as u32)
            .find(|&f| crate::face_normal(m, &m.faces[f as usize]).y > 0.9)
            .expect("a box has a top")
    }

    #[test]
    fn a_corner_to_corner_cut_splits_a_quad_into_two_triangles() {
        let mut m = box_mesh(Vec3::ONE);
        let f = top_face(&m);
        let ring = m.faces[f as usize].verts.clone();
        let KnifeCut { a, b, .. } = knife(&mut m, f, CutPoint::Vert(ring[0]), CutPoint::Vert(ring[2])).unwrap();
        assert_eq!(m.faces[a as usize].verts.len(), 3);
        assert_eq!(m.faces[b as usize].verts.len(), 3);
        assert_eq!(m.faces.len(), 7); // 6 - 1 + 2
        assert_eq!(m.verts.len(), 8, "a corner-to-corner cut adds no vertices");
        m.validate().unwrap();
    }

    /// The T-junction rule: cutting between two edge midpoints has to give the
    /// faces on the far side of those edges the new corner too, or the seam
    /// cracks.
    #[test]
    fn an_edge_to_edge_cut_also_splits_the_neighbours_edges() {
        let mut m = box_mesh(Vec3::ONE);
        let f = top_face(&m);
        let ring = m.faces[f as usize].verts.clone();
        let before: Vec<usize> = m.faces.iter().map(|f| f.verts.len()).collect();
        let KnifeCut { a, b, .. } = knife(
            &mut m,
            f,
            CutPoint::Edge { a: ring[0], b: ring[1], t: 0.5 },
            CutPoint::Edge { a: ring[2], b: ring[3], t: 0.5 },
        )
        .unwrap();
        assert_eq!(m.verts.len(), 10, "two new corners");
        assert_eq!(m.faces[a as usize].verts.len(), 4);
        assert_eq!(m.faces[b as usize].verts.len(), 4);
        // Exactly two OTHER faces (the walls under those two edges) grew by one
        // corner each; nothing else changed shape.
        let grew: Vec<u32> = (0..m.faces.len() as u32)
            .filter(|&i| i != a && i != b && (i as usize) < before.len())
            .filter(|&i| m.faces[i as usize].verts.len() > before[i as usize])
            .collect();
        assert_eq!(grew.len(), 2, "the two neighbouring walls must gain the cut corner");
        for i in grew {
            assert_eq!(m.faces[i as usize].verts.len(), 5);
        }
        m.validate().unwrap();
    }

    #[test]
    fn a_cut_that_cannot_divide_the_face_is_refused() {
        let mut m = box_mesh(Vec3::ONE);
        let f = top_face(&m);
        let ring = m.faces[f as usize].verts.clone();
        let before = m.clone();
        // Neighbouring corners: that "cut" is an edge the face already has.
        let e = knife(&mut m, f, CutPoint::Vert(ring[0]), CutPoint::Vert(ring[1])).unwrap_err();
        assert!(e.contains("already joined"), "{e}");
        // Both ends on one edge.
        let e = knife(
            &mut m,
            f,
            CutPoint::Edge { a: ring[0], b: ring[1], t: 0.25 },
            CutPoint::Edge { a: ring[1], b: ring[0], t: 0.75 },
        )
        .unwrap_err();
        assert!(e.contains("same edge"), "{e}");
        // The same corner twice.
        assert!(knife(&mut m, f, CutPoint::Vert(ring[0]), CutPoint::Vert(ring[0])).is_err());
        // A face that isn't there.
        assert!(knife(&mut m, 99, CutPoint::Vert(0), CutPoint::Vert(2)).is_err());
        // A refusal that only becomes visible AFTER the first point has split
        // an edge: mid-edge to the corner that edge starts at. The split must
        // be rolled back with it, or the mesh keeps a corner from a cut that
        // never happened.
        let e = knife(
            &mut m,
            f,
            CutPoint::Edge { a: ring[0], b: ring[1], t: 0.5 },
            CutPoint::Vert(ring[0]),
        )
        .unwrap_err();
        assert!(e.contains("already joined"), "{e}");
        assert_eq!(m, before, "a refused cut must not have edited anything");
    }

    /// A cut inherits the face's material slot on BOTH halves — cutting a
    /// differently-textured face must not repaint half of it.
    #[test]
    fn both_halves_keep_the_faces_slot() {
        let mut m = box_mesh(Vec3::ONE);
        m.slots.push("Trim".into());
        let f = top_face(&m);
        m.faces[f as usize].slot = 1;
        let ring = m.faces[f as usize].verts.clone();
        let KnifeCut { a, b, .. } = knife(&mut m, f, CutPoint::Vert(ring[0]), CutPoint::Vert(ring[2])).unwrap();
        assert_eq!(m.faces[a as usize].slot, 1);
        assert_eq!(m.faces[b as usize].slot, 1);
    }

    #[test]
    fn the_cursor_snaps_to_the_nearest_border_point_and_to_corners() {
        let m = box_mesh(Vec3::ONE);
        let f = top_face(&m);
        let ring = m.faces[f as usize].verts.clone();
        let c0 = m.verts[ring[0] as usize];
        let c1 = m.verts[ring[1] as usize];
        // Dead on a corner (and slightly inside it) → that corner.
        assert_eq!(nearest_cut_point(&m, f, c0, 0.2), Some(CutPoint::Vert(ring[0])));
        // Half way along an edge, nudged toward the middle of the face → that
        // edge at t = 0.5.
        let mid = (c0 + c1) * 0.5;
        match nearest_cut_point(&m, f, mid, 0.2).unwrap() {
            CutPoint::Edge { a, b, t } => {
                assert_eq!((a.min(b), a.max(b)), (ring[0].min(ring[1]), ring[0].max(ring[1])));
                assert!((t - 0.5).abs() < 1e-5, "t = {t}");
            }
            other => panic!("expected an edge point, got {other:?}"),
        }
        assert_eq!(nearest_cut_point(&m, 99, mid, 0.2), None);
    }

    /// `knife_refusal` answers exactly what `knife` would, without editing.
    ///
    /// This is what the editor's live preview asks so it can grey the cut line
    /// out while you are still aiming. If the two ever disagreed, the telegraph
    /// would promise a cut the click then refuses — the precise experience this
    /// check exists to end.
    #[test]
    fn the_preview_check_agrees_with_the_cut_itself() {
        let base = box_mesh(Vec3::ONE);
        let f = top_face(&base);
        let ring = base.faces[f as usize].verts.clone();
        let cases = [
            // Good: opposite corners, and edge to opposite edge.
            (CutPoint::Vert(ring[0]), CutPoint::Vert(ring[2])),
            (
                CutPoint::Edge { a: ring[0], b: ring[1], t: 0.5 },
                CutPoint::Edge { a: ring[2], b: ring[3], t: 0.5 },
            ),
            // Refused: neighbouring corners, both ends on one edge, the same
            // point twice, and mid-edge to a corner OF that edge.
            (CutPoint::Vert(ring[0]), CutPoint::Vert(ring[1])),
            (
                CutPoint::Edge { a: ring[0], b: ring[1], t: 0.25 },
                CutPoint::Edge { a: ring[1], b: ring[0], t: 0.75 },
            ),
            (CutPoint::Vert(ring[0]), CutPoint::Vert(ring[0])),
            (CutPoint::Edge { a: ring[0], b: ring[1], t: 0.5 }, CutPoint::Vert(ring[0])),
            (CutPoint::Edge { a: ring[0], b: ring[1], t: 0.5 }, CutPoint::Vert(ring[1])),
            // A cut running from an edge to a corner it is NOT adjacent to works.
            (CutPoint::Edge { a: ring[0], b: ring[1], t: 0.5 }, CutPoint::Vert(ring[3])),
            // An end snapped to a corner by END_SNAP is that corner, so this is
            // "neighbouring corners" in disguise.
            (CutPoint::Edge { a: ring[0], b: ring[1], t: 1e-6 }, CutPoint::Vert(ring[1])),
        ];
        for (p0, p1) in cases {
            let refusal = knife_refusal(&base, f, p0, p1);
            let mut m = base.clone();
            let cut = knife(&mut m, f, p0, p1);
            assert_eq!(
                refusal.is_some(),
                cut.is_err(),
                "preview and cut disagree for {p0:?} -> {p1:?}: refusal={refusal:?} cut={cut:?}"
            );
            if let (Some(a), Err(b)) = (refusal, cut) {
                assert_eq!(a, b, "the two must give the SAME reason");
            }
        }
        // A face index that isn't there: no opinion, and no panic.
        assert!(knife_refusal(&base, 99, CutPoint::Vert(0), CutPoint::Vert(2)).is_none());
    }

    /// The knife aims at a face's PLANE, not at whatever the ray hits first.
    ///
    /// Once the first point is placed the face is decided, so the second point
    /// has to be solved against that face even when the cursor has drifted past
    /// its edge or another face is nearer the camera. Re-picking there is what
    /// made a cut across a box corner restart on the neighbour instead of
    /// finishing.
    #[test]
    fn a_faces_plane_answers_even_when_the_ray_would_hit_something_else_first() {
        let m = box_mesh(Vec3::ONE);
        let top = top_face(&m);
        // Straight down onto the top: the plane hit is the top surface.
        let p = face_plane_hit(&m, top, Vec3::new(0.2, 5.0, 0.1), Vec3::NEG_Y).unwrap();
        assert!((p.y - 1.0).abs() < 1e-5, "{p}");
        // A ray aimed well OUTSIDE the face still answers on its plane — that is
        // what lets the cursor drift past an edge without the cut giving up.
        let p = face_plane_hit(&m, top, Vec3::new(9.0, 5.0, 0.0), Vec3::NEG_Y).unwrap();
        assert!((p.y - 1.0).abs() < 1e-5 && (p.x - 9.0).abs() < 1e-5, "{p}");
        // …and a ray that would hit the FAR face first still answers for this
        // one (the raycast would have returned the other face entirely).
        assert!(face_plane_hit(&m, top, Vec3::new(0.0, -5.0, 0.0), Vec3::Y).is_some());
        // Parallel to the plane: no answer, rather than an infinity.
        assert!(face_plane_hit(&m, top, Vec3::new(0.0, 5.0, 0.0), Vec3::X).is_none());
        // Pointing away from it: no answer either.
        assert!(face_plane_hit(&m, top, Vec3::new(0.0, 5.0, 0.0), Vec3::Y).is_none());
        assert!(face_plane_hit(&m, 99, Vec3::ZERO, Vec3::Y).is_none());
    }

    /// Chaining: cutting, then cutting again from the corner the first cut
    /// made, walks a groove across a face without leaving anything unwelded.
    #[test]
    fn cuts_chain_through_the_corner_the_last_one_made() {
        let mut m = box_mesh(Vec3::ONE);
        let f = top_face(&m);
        let ring = m.faces[f as usize].verts.clone();
        let cut = knife(
            &mut m,
            f,
            CutPoint::Edge { a: ring[0], b: ring[1], t: 0.5 },
            CutPoint::Vert(ring[2]),
        )
        .unwrap();
        let (a, end) = (cut.a, cut.v0);
        assert_eq!(m.verts[end as usize], m.verts[ring[0] as usize].lerp(m.verts[ring[1] as usize], 0.5));
        let ring2 = m.faces[a as usize].verts.clone();
        let other = ring2.iter().copied().find(|&v| v != end && v != ring[2]).unwrap();
        // Only cut when it actually divides — the point is that the mesh stays
        // valid through a chain.
        if knife(&mut m, a, CutPoint::Vert(end), CutPoint::Vert(other)).is_ok() {
            m.validate().unwrap();
        }
        m.validate().unwrap();
    }
}
