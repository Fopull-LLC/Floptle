//! Selection helpers: adjacency-driven grow / connected / coplanar face
//! selection and quad edge loops. Pure queries — nothing here mutates a mesh.
//!
//! All of them return deterministically ordered, deduped results (the editor
//! feeds them straight into a `BTreeSet`, and undo compares whole meshes).

use crate::{face_normal, MapMesh};
use glam::Vec3;
use std::collections::{BTreeSet, HashMap, HashSet};

/// Which corners and edges of `mesh` have at least one face turned toward
/// `eye` — everything else is round the back, hidden by the mesh's own front
/// surface.
///
/// `eye` is in the mesh's OWN object space; put the camera through the parent
/// chain's inverse rather than transforming every normal, so a rotated or
/// non-uniformly scaled node needs no inverse-transpose fix-up.
///
/// This is the cheap half of a hidden-line pass — O(faces), exact for a closed
/// convex shape and right for the overwhelming majority of blockout geometry.
/// It exists because a flat wireframe overlay draws a box's far rim exactly like
/// its near one, which makes the shape hard to read and makes it hard to tell
/// what a click is about to grab. The editor draws what this reports faintly,
/// but still draws it: seeing through a blockout is the point of the
/// see-through selection modes, so hiding it would take that away.
pub fn front_facing(mesh: &MapMesh, eye: Vec3) -> (Vec<bool>, HashSet<(u32, u32)>) {
    let mut verts = vec![false; mesh.verts.len()];
    let mut edges = HashSet::new();
    for f in &mesh.faces {
        if f.verts.len() < 3 || f.verts.iter().any(|&v| v as usize >= mesh.verts.len()) {
            continue;
        }
        let centre: Vec3 =
            f.verts.iter().map(|&v| mesh.verts[v as usize]).sum::<Vec3>() / f.verts.len() as f32;
        if face_normal(mesh, f).dot(eye - centre) <= 0.0 {
            continue; // turned away: it hides things, it doesn't reveal them
        }
        let n = f.verts.len();
        for i in 0..n {
            let (a, b) = (f.verts[i], f.verts[(i + 1) % n]);
            verts[a as usize] = true;
            edges.insert(key(a, b));
        }
    }
    // Nothing facing you means nothing is IN THE WAY, so the cue has no
    // meaning and everything draws normally. Two cases that matter, and both
    // are ordinary map editing rather than curiosities:
    //
    // * **Inside a room.** Build a box, stand in it — the commonest position
    //   there is — and every face is turned away. Dimming the whole wireframe
    //   there would make the tool worse exactly where it is used most.
    // * **A single-sided plane seen from behind.** One face, turned away,
    //   occluding nothing.
    //
    // Without this guard the "behind the surface" cue answers "all of it",
    // which is never information.
    if edges.is_empty() {
        return (vec![true; mesh.verts.len()], mesh.edges().into_iter().collect());
    }
    (verts, edges)
}

/// Canonical undirected edge key.
fn key(a: u32, b: u32) -> (u32, u32) {
    (a.min(b), a.max(b))
}

#[cfg(test)]
mod depth_cue_tests {
    use super::front_facing;
    use crate::box_mesh;
    use glam::Vec3;

    /// Looking at a box from +Z: the near face's four corners and edges are
    /// front-facing, the far ones are not. That distinction is the whole
    /// difference between a wireframe you can read and a flat tangle of
    /// identical lines.
    #[test]
    fn a_box_hides_its_far_side_from_wherever_you_stand() {
        let m = box_mesh(Vec3::ONE);
        for (eye, axis, sign) in [
            (Vec3::new(0.0, 0.0, 9.0), 2, 1.0),
            (Vec3::new(0.0, 0.0, -9.0), 2, -1.0),
            (Vec3::new(9.0, 0.0, 0.0), 0, 1.0),
            (Vec3::new(0.0, -9.0, 0.0), 1, -1.0),
        ] {
            let (verts, edges) = front_facing(&m, eye);
            // Every corner on the near side is visible; every corner on the far
            // side is not (a cube's corners are all at ±1).
            for (i, p) in m.verts.iter().enumerate() {
                let near = p[axis] * sign > 0.0;
                assert_eq!(
                    verts[i], near,
                    "corner {i} at {p} from {eye}: expected front={near}"
                );
            }
            // Straight on, exactly one face meets the eye — so the visible
            // edges are precisely that face's four, and every side edge
            // (one corner near, one far) is hidden along with the far rim.
            assert_eq!(edges.len(), 4, "from {eye}: one face meets the eye");
            for &(a, b) in &m.edges() {
                let both_near = m.verts[a as usize][axis] * sign > 0.0
                    && m.verts[b as usize][axis] * sign > 0.0;
                assert_eq!(edges.contains(&(a, b)), both_near, "edge {a}-{b} from {eye}");
            }
        }
    }

    /// A corner-on view sees three faces, so nothing but the single opposite
    /// corner is hidden — the cue degrades gracefully rather than flickering.
    #[test]
    fn a_corner_on_view_hides_only_the_opposite_corner() {
        let m = box_mesh(Vec3::ONE);
        let (verts, _) = front_facing(&m, Vec3::splat(9.0));
        let hidden: Vec<usize> = (0..m.verts.len()).filter(|&i| !verts[i]).collect();
        assert_eq!(hidden.len(), 1, "only (-1,-1,-1) is out of sight: {hidden:?}");
        assert_eq!(m.verts[hidden[0]], Vec3::splat(-1.0));
    }

    /// Inside a room, EVERYTHING reads as visible.
    ///
    /// Standing inside a box is the commonest position in map editing, and
    /// every face is turned away from you there. The literal answer — "all of
    /// it is behind the surface" — is true and useless: nothing is in the way,
    /// because the only faces that could hide anything are the ones behind the
    /// camera. Dimming the whole wireframe there would make the tool worse
    /// exactly where it is used most, so the guard says so.
    #[test]
    fn from_inside_a_room_everything_still_reads_as_visible() {
        let m = box_mesh(Vec3::ONE);
        let (verts, edges) = front_facing(&m, Vec3::ZERO);
        assert!(verts.iter().all(|v| *v), "inside a box, nothing occludes anything");
        assert_eq!(edges.len(), m.edges().len(), "every edge stays fully drawn");
    }

    /// The same guard covers a single-sided plane seen from behind: one face,
    /// turned away, occluding nothing.
    #[test]
    fn a_one_sided_plane_seen_from_behind_is_not_dimmed() {
        let m = crate::plane(glam::Vec2::new(4.0, 4.0));
        let front = front_facing(&m, Vec3::new(0.0, 5.0, 0.0));
        let back = front_facing(&m, Vec3::new(0.0, -5.0, 0.0));
        assert!(front.0.iter().all(|v| *v), "from the front, obviously");
        assert!(back.0.iter().all(|v| *v), "…and from behind, since nothing hides it");
        assert_eq!(back.1.len(), m.edges().len());
    }

    /// An empty mesh answers empty rather than panicking.
    #[test]
    fn an_empty_mesh_answers_empty() {
        let m = box_mesh(Vec3::ONE);
        let empty = crate::MapMesh { verts: Vec::new(), faces: Vec::new(), ..m.clone() };
        let (verts, edges) = front_facing(&empty, Vec3::ZERO);
        assert!(verts.is_empty());
        assert!(edges.is_empty());
    }
}

/// Which faces use each undirected edge.
fn edge_faces(mesh: &MapMesh) -> HashMap<(u32, u32), Vec<u32>> {
    let mut out: HashMap<(u32, u32), Vec<u32>> = HashMap::new();
    for (fi, f) in mesh.faces.iter().enumerate() {
        let k = f.verts.len();
        if k < 3 {
            continue;
        }
        for i in 0..k {
            out.entry(key(f.verts[i], f.verts[(i + 1) % k])).or_default().push(fi as u32);
        }
    }
    out
}

/// The selection plus every face sharing an edge with it (one ring).
pub fn grow_faces(mesh: &MapMesh, faces: &[u32]) -> Vec<u32> {
    let adj = edge_faces(mesh);
    let mut out: BTreeSet<u32> =
        faces.iter().copied().filter(|&f| (f as usize) < mesh.faces.len()).collect();
    for &f in &out.clone() {
        let face = &mesh.faces[f as usize];
        let k = face.verts.len();
        for i in 0..k {
            if let Some(fs) = adj.get(&key(face.verts[i], face.verts[(i + 1) % k])) {
                out.extend(fs.iter().copied());
            }
        }
    }
    out.into_iter().collect()
}

/// The selection minus its outer ring: every face that still has a neighbour outside
/// the selection is dropped. The counterpart to [`grow_faces`], and the reason grow was
/// only half a tool — you could widen a selection and never narrow it again.
pub fn shrink_faces(mesh: &MapMesh, faces: &[u32]) -> Vec<u32> {
    let adj = edge_faces(mesh);
    let sel: BTreeSet<u32> =
        faces.iter().copied().filter(|&f| (f as usize) < mesh.faces.len()).collect();
    sel.iter()
        .copied()
        .filter(|&f| {
            let face = &mesh.faces[f as usize];
            let k = face.verts.len();
            (0..k).all(|i| {
                // A border edge (only this face uses it) is an outside too — a face on
                // the mesh's rim is on the selection's rim.
                match adj.get(&key(face.verts[i], face.verts[(i + 1) % k])) {
                    Some(fs) => fs.len() > 1 && fs.iter().all(|n| sel.contains(n)),
                    None => false,
                }
            })
        })
        .collect()
}

/// Faces whose corners no longer lie in one plane, by more than `tol` (local units of
/// deviation from the face's own best-fit plane).
///
/// Diagnostic, and the reason it exists: a warped face is the thing that *looks* wrong
/// after an edit, and until now there was no way to find one except by eye. Triangles
/// and faces with no area are never reported — a triangle is planar by definition.
pub fn non_planar_faces(mesh: &MapMesh, tol: f32) -> Vec<u32> {
    let mut out = Vec::new();
    for (fi, f) in mesh.faces.iter().enumerate() {
        if f.verts.len() < 4 {
            continue;
        }
        let n = crate::face_normal(mesh, f);
        let Some(&v0) = f.verts.first() else { continue };
        let o = mesh.verts[v0 as usize];
        let worst = f
            .verts
            .iter()
            .map(|&v| (mesh.verts[v as usize] - o).dot(n).abs())
            .fold(0.0f32, f32::max);
        if worst > tol {
            out.push(fi as u32);
        }
    }
    out
}

/// Every face reachable from the selection across shared edges (the whole
/// connected shell).
pub fn connected_faces(mesh: &MapMesh, faces: &[u32]) -> Vec<u32> {
    flood(mesh, faces, |_, _| true)
}

/// Flood-fill from the selection across shared edges, keeping only faces whose
/// normal stays within `tol_deg` of the face it spread from AND which lie in
/// the same plane (so a blockout's whole floor selects, but not the wall it
/// meets, and not a parallel floor one storey up).
pub fn coplanar_faces(mesh: &MapMesh, faces: &[u32], tol_deg: f32) -> Vec<u32> {
    let cos_tol = tol_deg.to_radians().cos();
    flood(mesh, faces, move |a, b| {
        let (na, nb) = (a.1, b.1);
        na.dot(nb) >= cos_tol && (b.0 - a.0).dot(na).abs() <= 1e-3
    })
}

/// Shared flood fill; `accept(from, to)` gets each face's (centroid, normal).
fn flood(
    mesh: &MapMesh,
    faces: &[u32],
    accept: impl Fn((glam::Vec3, glam::Vec3), (glam::Vec3, glam::Vec3)) -> bool,
) -> Vec<u32> {
    let adj = edge_faces(mesh);
    let info = |f: u32| {
        let face = &mesh.faces[f as usize];
        let c = face.verts.iter().map(|&v| mesh.verts[v as usize]).sum::<glam::Vec3>()
            / face.verts.len() as f32;
        (c, face_normal(mesh, face))
    };
    let mut out: BTreeSet<u32> =
        faces.iter().copied().filter(|&f| (f as usize) < mesh.faces.len()).collect();
    let mut stack: Vec<u32> = out.iter().copied().collect();
    while let Some(f) = stack.pop() {
        let from = info(f);
        let face = &mesh.faces[f as usize];
        let k = face.verts.len();
        for i in 0..k {
            let Some(fs) = adj.get(&key(face.verts[i], face.verts[(i + 1) % k])) else { continue };
            for &n in fs {
                if out.contains(&n) || !accept(from, info(n)) {
                    continue;
                }
                out.insert(n);
                stack.push(n);
            }
        }
    }
    out.into_iter().collect()
}

/// Faces drawing with material slot `slot`.
pub fn faces_with_slot(mesh: &MapMesh, slot: u16) -> Vec<u32> {
    let n = mesh.slots.len() as u16;
    mesh.faces
        .iter()
        .enumerate()
        .filter(|(_, f)| if f.slot < n { f.slot } else { 0 } == slot)
        .map(|(i, _)| i as u32)
        .collect()
}

/// The quad edge loop through `edge`: walk both ways, at each vertex continuing
/// onto the edge that belongs to NEITHER face of the edge we arrived on (the
/// standard rule — it only continues through 4-valence quad junctions, so it
/// stops cleanly at a triangle fan, a pole, or an open border).
pub fn edge_loop(mesh: &MapMesh, edge: (u32, u32)) -> Vec<(u32, u32)> {
    let adj = edge_faces(mesh);
    let start = key(edge.0, edge.1);
    if !adj.contains_key(&start) {
        return Vec::new();
    }
    // Edges meeting at each vertex.
    let mut at_vert: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
    for &e in adj.keys() {
        at_vert.entry(e.0).or_default().push(e);
        at_vert.entry(e.1).or_default().push(e);
    }
    for v in at_vert.values_mut() {
        v.sort_unstable();
    }
    let mut out: BTreeSet<(u32, u32)> = BTreeSet::new();
    out.insert(start);
    for &first_dir in &[false, true] {
        let (mut cur, mut vert) = (start, if first_dir { start.0 } else { start.1 });
        for _ in 0..mesh.faces.len().max(4) * 4 {
            let cur_faces = adj.get(&cur).cloned().unwrap_or_default();
            // Only continue through a quad junction: exactly 4 edges at the
            // vertex, exactly one of which touches neither adjacent face.
            let Some(next) = at_vert.get(&vert).and_then(|es| {
                if es.len() != 4 {
                    return None;
                }
                let mut cands = es.iter().copied().filter(|&e| {
                    e != cur
                        && adj
                            .get(&e)
                            .is_some_and(|fs| fs.iter().all(|f| !cur_faces.contains(f)))
                });
                let first = cands.next()?;
                cands.next().is_none().then_some(first)
            }) else {
                break;
            };
            if !out.insert(next) {
                break; // closed the loop
            }
            vert = if next.0 == vert { next.1 } else { next.0 };
            cur = next;
        }
    }
    out.into_iter().collect()
}

/// The edge RING through `edge`: the parallel edges a strip of quads is
/// crossed by, stepping across each quad to the edge opposite the one we came
/// in on.
///
/// Not to be confused with [`edge_loop`], which is the chain of edges running
/// END TO END. The two are perpendicular to each other and both are called a
/// "loop" in casual speech; the ring is the one a loop CUT needs, because the
/// cut runs across every edge in it.
///
/// Only steps through quads — a triangle has no "opposite" edge — so it stops
/// cleanly at a fan, a pole or a border, and returns what it walked. Closes on
/// itself around a cylinder without looping forever.
pub fn edge_ring(mesh: &MapMesh, edge: (u32, u32)) -> Vec<(u32, u32)> {
    let start = key(edge.0, edge.1);
    let adj = edge_faces(mesh);
    if !adj.contains_key(&start) {
        return Vec::new();
    }
    // The edge opposite `e` in quad `f`, if `f` is a quad that has `e`.
    let across = |f: u32, e: (u32, u32)| -> Option<(u32, u32)> {
        let face = &mesh.faces[f as usize];
        if face.verts.len() != 4 {
            return None;
        }
        let i = (0..4).find(|&i| key(face.verts[i], face.verts[(i + 1) % 4]) == e)?;
        let j = (i + 2) % 4;
        Some(key(face.verts[j], face.verts[(j + 1) % 4]))
    };
    let mut seen: BTreeSet<(u32, u32)> = BTreeSet::new();
    let mut out = vec![start];
    seen.insert(start);
    // Walk out through each of the starting edge's faces in turn. Two passes
    // cover both directions along the strip; a closed ring simply runs into
    // something already seen and stops.
    for &f0 in adj.get(&start).into_iter().flatten() {
        let (mut face, mut e) = (f0, start);
        while let Some(next) = across(face, e) {
            if !seen.insert(next) {
                break;
            }
            out.push(next);
            // Step to the OTHER face of the edge we just crossed onto.
            let Some(fs) = adj.get(&next) else { break };
            let Some(&nf) = fs.iter().find(|&&x| x != face) else { break };
            face = nf;
            e = next;
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{box_mesh, cylinder, plane, subdivide_faces};
    use glam::{Vec2, Vec3};

    /// A flat n×n grid of quads in the XZ plane, vertices row-major so the
    /// topology is predictable: vertex (r,c) is `r*(n+1)+c`, face (r,c) is
    /// `r*n+c`.
    fn grid(n: u32) -> MapMesh {
        let mut m = MapMesh::new();
        for r in 0..=n {
            for c in 0..=n {
                m.verts.push(Vec3::new(c as f32, 0.0, r as f32));
            }
        }
        let vid = |r: u32, c: u32| r * (n + 1) + c;
        for r in 0..n {
            for c in 0..n {
                m.faces.push(crate::Face {
                    verts: vec![vid(r, c), vid(r, c + 1), vid(r + 1, c + 1), vid(r + 1, c)],
                    slot: 0,
                });
            }
        }
        m
    }

    /// Ctrl+click across a grid: the path is the straight run between the two
    /// picks, both ends included, and nothing else.
    #[test]
    fn a_vertex_path_runs_straight_along_a_row() {
        let m = grid(4);
        let vid = |r: u32, c: u32| r * 5 + c;
        let path = path_verts(&m, vid(0, 0), vid(0, 4));
        assert_eq!(path, vec![vid(0, 0), vid(0, 1), vid(0, 2), vid(0, 3), vid(0, 4)]);
    }

    /// The route is the CHEAPEST one, not the first one found: a diagonal pick
    /// takes 8 steps across a 4x4 grid however it staircases, and must start and
    /// end exactly where it was told to.
    #[test]
    fn a_vertex_path_across_a_diagonal_is_a_shortest_staircase() {
        let m = grid(4);
        let vid = |r: u32, c: u32| r * 5 + c;
        let path = path_verts(&m, vid(0, 0), vid(4, 4));
        assert_eq!(path.first(), Some(&vid(0, 0)));
        assert_eq!(path.last(), Some(&vid(4, 4)));
        assert_eq!(path.len(), 9, "8 unit steps, 9 vertices: {path:?}");
        // Every consecutive pair is one grid unit apart — a real walk, not a jump.
        for w in path.windows(2) {
            let d = (m.verts[w[1] as usize] - m.verts[w[0] as usize]).length();
            assert!((d - 1.0).abs() < 1e-5, "step of {d} in {path:?}");
        }
    }

    /// A strip of faces along a row — the "select this limb" gesture.
    #[test]
    fn a_face_path_walks_a_strip() {
        let m = grid(4);
        let path = path_faces(&m, 0, 3);
        assert_eq!(path, vec![0, 1, 2, 3]);
    }

    /// Edges step through shared vertices.
    #[test]
    fn an_edge_path_runs_along_a_seam() {
        let m = grid(4);
        let vid = |r: u32, c: u32| r * 5 + c;
        let path = path_edges(&m, (vid(0, 0), vid(0, 1)), (vid(0, 3), vid(0, 4)));
        assert_eq!(path.first(), Some(&(vid(0, 0), vid(0, 1))));
        assert_eq!(path.last(), Some(&(vid(0, 3), vid(0, 4))));
        assert_eq!(path.len(), 4);
    }

    /// Clicking across a gap between two separate shells has no route. It must
    /// answer "no path" rather than an arbitrary one, so the caller can fall
    /// back to an ordinary pick.
    #[test]
    fn there_is_no_path_between_separate_shells() {
        let mut m = box_mesh(Vec3::ONE);
        let far = plane(Vec2::ONE);
        crate::merge_into(&mut m, &far, &glam::Mat4::from_translation(Vec3::Y * 40.0));
        assert!(path_faces(&m, 0, 6).is_empty());
    }

    /// Picking the same element twice is a path of one — never a crash, never
    /// an empty answer that would read as "no route".
    #[test]
    fn a_path_to_itself_is_just_itself() {
        let m = grid(2);
        assert_eq!(path_verts(&m, 0, 0), vec![0]);
        assert_eq!(path_faces(&m, 1, 1), vec![1]);
    }

    /// Out-of-range indices (a stale selection after an op reindexed the mesh)
    /// answer "no path" instead of panicking.
    #[test]
    fn a_stale_index_is_refused_rather_than_indexed() {
        let m = grid(2);
        assert!(path_verts(&m, 0, 9999).is_empty());
        assert!(path_faces(&m, 9999, 0).is_empty());
        assert!(path_edges(&m, (0, 1), (900, 901)).is_empty());
    }

    #[test]
    fn grow_reaches_the_four_neighbours_of_a_box_face() {
        let m = box_mesh(Vec3::ONE);
        let grown = grow_faces(&m, &[0]);
        assert_eq!(grown.len(), 5); // the face + 4 side neighbours (not the far one)
        assert!(grown.contains(&0));
    }

    #[test]
    fn connected_takes_the_whole_shell_and_nothing_else() {
        let mut m = box_mesh(Vec3::ONE);
        let far = plane(Vec2::ONE);
        crate::merge_into(&mut m, &far, &glam::Mat4::from_translation(Vec3::Y * 40.0));
        assert_eq!(connected_faces(&m, &[0]).len(), 6);
        assert_eq!(connected_faces(&m, &[6]), vec![6]);
    }

    #[test]
    fn coplanar_stops_at_the_edge_of_a_flat_region() {
        // A subdivided plane: every quad is coplanar, so one pick takes them all.
        let mut m = plane(Vec2::splat(2.0));
        let quads = subdivide_faces(&mut m, &[0]);
        assert_eq!(coplanar_faces(&m, &[quads[0]], 5.0).len(), 4);
        // On a box, a face's neighbours are all 90 degrees away.
        let b = box_mesh(Vec3::ONE);
        assert_eq!(coplanar_faces(&b, &[0], 5.0), vec![0]);
    }

    #[test]
    fn edge_loop_crosses_a_quad_grid() {
        // 4x4 grid of quads: every grid line is a 4-edge loop, and the walk
        // stops at the border (valence 3) instead of turning a corner.
        let mut m = plane(Vec2::splat(2.0));
        let q = subdivide_faces(&mut m, &[0]);
        subdivide_faces(&mut m, &q);
        let center = m.verts.iter().position(|v| v.length() < 1e-6).unwrap() as u32;
        let spoke = m.edges().into_iter().find(|e| e.0 == center || e.1 == center).unwrap();
        // Runs from border to border through the two 4-valence junctions.
        assert_eq!(edge_loop(&m, spoke).len(), 4);
        // A border edge dead-ends at the grid's corner (valence 2).
        let corner = m.verts.iter().position(|v| v.x.abs() > 1.9 && v.z.abs() > 1.9).unwrap() as u32;
        let border = m.edges().into_iter().find(|e| e.0 == corner || e.1 == corner).unwrap();
        assert_eq!(edge_loop(&m, border), vec![border]);
    }

    #[test]
    fn edge_loop_stops_at_a_non_quad_junction() {
        // A cylinder's cap ring is a fan of triangles round an n-gon: the
        // valence-3 rim gives the walk nowhere to go, so it stays put.
        let m = cylinder(1.0, 1.0, 8);
        let wall = m.faces[2].clone();
        let e = (wall.verts[0].min(wall.verts[1]), wall.verts[0].max(wall.verts[1]));
        assert_eq!(edge_loop(&m, e), vec![e]);
    }

    #[test]
    fn edge_loop_of_an_unknown_edge_is_empty() {
        let m = box_mesh(Vec3::ONE);
        assert!(edge_loop(&m, (0, 6)).is_empty());
    }

    #[test]
    fn slot_query_groups_faces() {
        let mut m = box_mesh(Vec3::ONE);
        m.slots.push("Wall".into());
        crate::set_face_slot(&mut m, &[1, 3], 1);
        assert_eq!(faces_with_slot(&m, 1), vec![1, 3]);
        assert_eq!(faces_with_slot(&m, 0).len(), 4);
    }
}

// ---- shortest path between two picked elements --------------------------------
//
// Blender's "pick shortest path": click one element, Ctrl+click another, and
// everything on the cheapest route between them joins the selection. It is how
// you select a seam around a shape, or a strip of faces along a limb, without
// clicking each one — and it works in all three sub-object modes.
//
// Cost is the EUCLIDEAN distance between neighbouring element centres, not a hop
// count. On an even grid the two agree; on anything irregular, hop count happily
// routes the long way round through a dense patch of small faces because it can
// do it in fewer steps, which is never what the eye expects.

/// Dijkstra over a graph given as `neighbours(node) -> [(node, centre)]`, from
/// `start` to `goal`, returning the nodes on the path INCLUSIVE of both ends.
/// Empty when no route exists (separate shells) — the caller falls back to a
/// plain pick, which is what a user who clicked across a gap meant anyway.
fn dijkstra<N, F, C>(start: N, goal: N, centre: C, mut neighbours: F) -> Vec<N>
where
    N: Copy + Ord + std::hash::Hash,
    F: FnMut(N, &mut Vec<N>),
    C: Fn(N) -> Vec3,
{
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    if start == goal {
        return vec![start];
    }
    // f32 has no Ord, and the cost is always finite and non-negative here, so
    // ordering by the bit pattern of the bits-as-u32 is exact for our purposes.
    let bits = |x: f32| x.to_bits();
    let mut dist: HashMap<N, f32> = HashMap::new();
    let mut prev: HashMap<N, N> = HashMap::new();
    let mut heap: BinaryHeap<Reverse<(u32, N)>> = BinaryHeap::new();
    dist.insert(start, 0.0);
    heap.push(Reverse((bits(0.0), start)));
    let mut buf = Vec::new();
    while let Some(Reverse((d, n))) = heap.pop() {
        let d = f32::from_bits(d);
        if n == goal {
            break;
        }
        if dist.get(&n).is_some_and(|best| d > *best) {
            continue;
        }
        let c = centre(n);
        buf.clear();
        neighbours(n, &mut buf);
        for &m in &buf {
            let nd = d + (centre(m) - c).length();
            if dist.get(&m).is_none_or(|best| nd < *best) {
                dist.insert(m, nd);
                prev.insert(m, n);
                heap.push(Reverse((bits(nd), m)));
            }
        }
    }
    if !dist.contains_key(&goal) {
        return Vec::new();
    }
    let mut path = vec![goal];
    let mut cur = goal;
    while let Some(&p) = prev.get(&cur) {
        path.push(p);
        cur = p;
        if cur == start {
            break;
        }
    }
    if cur != start {
        return Vec::new();
    }
    path.reverse();
    path
}

/// Every vertex on the cheapest edge route from `from` to `to`, both included.
pub fn path_verts(mesh: &MapMesh, from: u32, to: u32) -> Vec<u32> {
    let n = mesh.verts.len() as u32;
    if from >= n || to >= n {
        return Vec::new();
    }
    let mut adj: HashMap<u32, Vec<u32>> = HashMap::new();
    for (a, b) in mesh.edges() {
        adj.entry(a).or_default().push(b);
        adj.entry(b).or_default().push(a);
    }
    dijkstra(
        from,
        to,
        |v| mesh.verts[v as usize],
        |v, out| out.extend(adj.get(&v).into_iter().flatten().copied()),
    )
}

/// Every face on the cheapest route from `from` to `to` across shared EDGES —
/// a strip of faces, the way you would walk them.
pub fn path_faces(mesh: &MapMesh, from: u32, to: u32) -> Vec<u32> {
    let n = mesh.faces.len() as u32;
    if from >= n || to >= n {
        return Vec::new();
    }
    let shared = edge_faces(mesh);
    let centre = |f: u32| {
        let face = &mesh.faces[f as usize];
        face.verts.iter().map(|&v| mesh.verts[v as usize]).sum::<Vec3>() / face.verts.len() as f32
    };
    dijkstra(from, to, centre, |f, out| {
        let face = &mesh.faces[f as usize];
        let k = face.verts.len();
        for i in 0..k {
            let Some(fs) = shared.get(&key(face.verts[i], face.verts[(i + 1) % k])) else {
                continue;
            };
            out.extend(fs.iter().copied().filter(|&m| m != f));
        }
    })
}

/// Every edge on the cheapest route from `from` to `to`, stepping between edges
/// that share a vertex. Edges are canonical `(a < b)` pairs, as everywhere else.
pub fn path_edges(mesh: &MapMesh, from: (u32, u32), to: (u32, u32)) -> Vec<(u32, u32)> {
    let all = mesh.edges();
    let (from, to) = (key(from.0, from.1), key(to.0, to.1));
    if !all.contains(&from) || !all.contains(&to) {
        return Vec::new();
    }
    let mut at_vert: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
    for &e in &all {
        at_vert.entry(e.0).or_default().push(e);
        at_vert.entry(e.1).or_default().push(e);
    }
    let centre =
        |e: (u32, u32)| (mesh.verts[e.0 as usize] + mesh.verts[e.1 as usize]) * 0.5;
    dijkstra(from, to, centre, |e, out| {
        for v in [e.0, e.1] {
            out.extend(at_vert.get(&v).into_iter().flatten().copied().filter(|&m| m != e));
        }
    })
}
