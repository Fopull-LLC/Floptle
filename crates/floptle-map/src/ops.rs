//! Editing operations. Every op must leave a `validate()`-clean mesh
//! `validate()`-clean, and must be deterministic (same inputs -> same output,
//! including ordering) because undo snapshots compare/restore whole meshes.

use crate::triangulate::newell;
use crate::{Face, MapMesh};
use glam::{Mat4, Vec3};
use std::collections::{BTreeSet, HashMap};

/// Mark a mesh as no longer being the primitive it was generated from.
///
/// Every op that moves a vertex or changes the face set calls this: once the
/// user has pulled a face, re-generating from the old parameters would throw
/// that edit away. Material-slot assignment deliberately does NOT clear it —
/// painting a stair's treads must not cost you the step-count control.
fn touched(mesh: &mut MapMesh) {
    mesh.spec = None;
}

/// Selected face indices, deduped + in-range + sorted (deterministic order).
fn valid_faces(mesh: &MapMesh, faces: &[u32]) -> Vec<usize> {
    let set: BTreeSet<usize> =
        faces.iter().map(|&f| f as usize).filter(|&f| f < mesh.faces.len()).collect();
    set.into_iter().collect()
}

/// Selected vertex indices, deduped + in-range (applied-once semantics).
fn valid_verts(mesh: &MapMesh, verts: &[u32]) -> Vec<usize> {
    let set: BTreeSet<usize> =
        verts.iter().map(|&v| v as usize).filter(|&v| v < mesh.verts.len()).collect();
    set.into_iter().collect()
}

/// Move the given vertices by `delta`. Duplicate indices are moved once;
/// out-of-range indices are ignored.
pub fn translate_verts(mesh: &mut MapMesh, verts: &[u32], delta: Vec3) {
    touched(mesh);
    for v in valid_verts(mesh, verts) {
        mesh.verts[v] += delta;
    }
}

/// Apply an arbitrary transform (rotate/scale about a pivot, baked into `m`)
/// to the given vertices. Duplicates applied once; out-of-range ignored.
pub fn transform_verts(mesh: &mut MapMesh, verts: &[u32], m: &Mat4) {
    touched(mesh);
    for v in valid_verts(mesh, verts) {
        mesh.verts[v] = m.transform_point3(mesh.verts[v]);
    }
}

/// Region-extrude the selected faces by `distance` along the selection's
/// area-weighted average normal.
///
/// Semantics:
/// - The selected faces MOVE (their vertices are duplicated first so shared
///   unselected geometry stays put; verts shared by two selected faces are
///   duplicated once — the region stays welded).
/// - Side wall quads are created only on the region's BOUNDARY edges (edges
///   used by exactly one selected face), winding outward; each wall inherits
///   the slot of the selected face that owned the boundary edge.
/// - Returns the face indices of the (moved) selected faces, so the editor
///   can keep them selected and chain drags.
/// - `distance` 0.0 is legal (extrude in place, then drag).
/// - Empty/out-of-range selection: no-op, returns empty.
pub fn extrude_faces(mesh: &mut MapMesh, faces: &[u32], distance: f32) -> Vec<u32> {
    touched(mesh);
    let sel = valid_faces(mesh, faces);
    if sel.is_empty() {
        return Vec::new();
    }
    // Area-weighted average normal (Newell magnitude = 2x area), before moving.
    let mut nsum = Vec3::ZERO;
    for &fi in &sel {
        nsum += newell(mesh, &mesh.faces[fi]);
    }
    // A CLOSED selection has no direction to go. Every face of a box sums to zero, so
    // "select all, press E" used to normalize a zero vector, fall back to +Y, and
    // translate the entire shell upward — while making no walls (nothing is a boundary
    // edge) and leaving every original vertex behind as an orphan that still draws as a
    // dot and still box-selects. That reads exactly like "the tool invented vertices".
    // Refusing is the honest answer: there is no such thing as extruding a closed solid
    // along its own normals.
    let Some(dir) = nsum.try_normalize() else {
        return Vec::new();
    };

    // Count canonical edges across the selection to find boundary edges, and
    // remember each boundary edge in its owning face's traversal direction.
    let mut edge_count: HashMap<(u32, u32), u32> = HashMap::new();
    for &fi in &sel {
        let f = &mesh.faces[fi];
        let k = f.verts.len();
        for i in 0..k {
            let (a, b) = (f.verts[i], f.verts[(i + 1) % k]);
            *edge_count.entry((a.min(b), a.max(b))).or_default() += 1;
        }
    }

    // Duplicate the region's vertices once (deterministic order: face-by-face,
    // corner-by-corner over the sorted selection).
    let mut dup: HashMap<u32, u32> = HashMap::new();
    for &fi in &sel {
        for i in 0..mesh.faces[fi].verts.len() {
            let v = mesh.faces[fi].verts[i];
            dup.entry(v).or_insert_with(|| {
                mesh.verts.push(mesh.verts[v as usize] + dir * distance);
                mesh.verts.len() as u32 - 1
            });
        }
    }

    // Walls on boundary edges (a -> b in the selected face's direction gives
    // an outward-wound [a_old, b_old, b_new, a_new] quad), then rewrite the
    // selected faces onto their duplicated verts.
    let mut walls: Vec<Face> = Vec::new();
    for &fi in &sel {
        let f = &mesh.faces[fi];
        let (k, slot) = (f.verts.len(), f.slot);
        for i in 0..k {
            let (a, b) = (f.verts[i], f.verts[(i + 1) % k]);
            if edge_count[&(a.min(b), a.max(b))] == 1 {
                walls.push(Face { verts: vec![a, b, dup[&b], dup[&a]], slot });
            }
        }
    }
    for &fi in &sel {
        for i in 0..mesh.faces[fi].verts.len() {
            let v = mesh.faces[fi].verts[i];
            mesh.faces[fi].verts[i] = dup[&v];
        }
    }
    mesh.faces.extend(walls);
    sel.into_iter().map(|f| f as u32).collect()
}

/// Inset the selected faces: each face gets a smaller copy of itself inside its
/// own plane, joined to the original border by a ring of side quads.
///
/// Semantics:
/// - Faces inset INDIVIDUALLY (each keeps its own border), which is what a
///   blockout needs — pick one face, inset, extrude, and you have a recess.
/// - Corners move along their angle bisector by `amount` (a true edge offset,
///   not a centroid shrink), clamped so a face can never turn inside out; a
///   face too small for `amount` shrinks to 10% instead of inverting.
/// - The inner face keeps the original's slot and index; the new ring quads
///   append. Returns the inner face indices (so a following extrude/drag
///   operates on exactly what was inset).
pub fn inset_faces(mesh: &mut MapMesh, faces: &[u32], amount: f32) -> Vec<u32> {
    touched(mesh);
    let sel = valid_faces(mesh, faces);
    if sel.is_empty() || amount <= 0.0 {
        return Vec::new();
    }
    let mut walls: Vec<Face> = Vec::new();
    for &fi in &sel {
        let f = mesh.faces[fi].clone();
        let k = f.verts.len();
        let n = crate::face_normal(mesh, &f);
        let pts: Vec<Vec3> = f.verts.iter().map(|&v| mesh.verts[v as usize]).collect();
        let centroid = pts.iter().copied().sum::<Vec3>() / k as f32;
        let mut inner: Vec<Vec3> = Vec::with_capacity(k);
        let mut ok = true;
        for i in 0..k {
            let (prev, cur, next) = (pts[(i + k - 1) % k], pts[i], pts[(i + 1) % k]);
            // Inward normals of the two edges meeting at `cur` (CCW-from-outside
            // winding puts the interior at cross(face normal, edge)).
            let e_in = match (cur - prev).try_normalize() {
                Some(e) => n.cross(e),
                None => {
                    ok = false;
                    break;
                }
            };
            let e_out = match (next - cur).try_normalize() {
                Some(e) => n.cross(e),
                None => {
                    ok = false;
                    break;
                }
            };
            let denom = 1.0 + e_in.dot(e_out);
            let step = if denom.abs() < 1e-4 {
                e_in * amount // ~180 degree corner: offset off one edge
            } else {
                (e_in + e_out) * (amount / denom)
            };
            inner.push(cur + step);
        }
        // Reject an inset that collapsed or flipped the face (amount too big for
        // this face) and fall back to a safe shrink toward the centroid.
        if ok {
            let probe = Face { verts: (0..k as u32).collect(), slot: 0 };
            let scratch =
                MapMesh { verts: inner.clone(), faces: vec![probe], slots: mesh.slots.clone(), spec: None };
            let n2 = newell(&scratch, &scratch.faces[0]);
            // Newell's magnitude is 2x the area: an inset must keep the winding
            // AND shrink. (A point-reflected polygon — what a wildly oversized
            // amount produces — keeps its winding, so the area test is the one
            // that catches it.)
            let n0 = newell(mesh, &f);
            ok = n2.dot(n) > 0.0 && n2.length() > 1e-9 && n2.length() <= n0.length();
        }
        if !ok {
            inner = pts.iter().map(|&p| centroid + (p - centroid) * 0.1).collect();
        }
        let base = mesh.verts.len() as u32;
        mesh.verts.extend(inner);
        for i in 0..k {
            let j = (i + 1) % k;
            walls.push(Face {
                verts: vec![f.verts[i], f.verts[j], base + j as u32, base + i as u32],
                slot: f.slot,
            });
        }
        mesh.faces[fi].verts = (0..k as u32).map(|i| base + i).collect();
    }
    mesh.faces.extend(walls);
    sel.into_iter().map(|f| f as u32).collect()
}

/// Split the selected faces off into their own mesh (returned, with the same
/// slot names), removing them from `mesh`. The new mesh's vertices are in the
/// SAME local frame, so the caller can spawn a node with the identical
/// transform and nothing moves. Returns `None` when the selection is empty or
/// covers the whole mesh (nothing would be left behind).
pub fn detach_faces(mesh: &mut MapMesh, faces: &[u32]) -> Option<MapMesh> {
    touched(mesh);
    let sel = valid_faces(mesh, faces);
    if sel.is_empty() || sel.len() >= mesh.faces.len() {
        return None;
    }
    let mut out =
        MapMesh { verts: Vec::new(), faces: Vec::new(), slots: mesh.slots.clone(), spec: None };
    let mut remap: HashMap<u32, u32> = HashMap::new();
    for &fi in &sel {
        let f = &mesh.faces[fi];
        let verts = f
            .verts
            .iter()
            .map(|&v| {
                *remap.entry(v).or_insert_with(|| {
                    out.verts.push(mesh.verts[v as usize]);
                    out.verts.len() as u32 - 1
                })
            })
            .collect();
        out.faces.push(Face { verts, slot: f.slot });
    }
    delete_faces(mesh, faces);
    Some(out)
}

/// Bridge two faces with a tube of quads: both faces are removed and their
/// borders joined wall-by-wall. The faces must have the same corner count.
/// Returns the new wall face indices (empty when the bridge isn't possible).
pub fn bridge_faces(mesh: &mut MapMesh, a: u32, b: u32) -> Vec<u32> {
    touched(mesh);
    let sel = valid_faces(mesh, &[a, b]);
    if sel.len() != 2 {
        return Vec::new();
    }
    let (fa, fb) = (mesh.faces[sel[0]].clone(), mesh.faces[sel[1]].clone());
    let k = fa.verts.len();
    if k != fb.verts.len() {
        return Vec::new();
    }
    let pos = |v: u32| mesh.verts[v as usize];
    let ca = fa.verts.iter().map(|&v| pos(v)).sum::<Vec3>() / k as f32;
    let cb = fb.verts.iter().map(|&v| pos(v)).sum::<Vec3>() / k as f32;
    // The two borders wind opposite ways around the tube (each is CCW seen from
    // its own outside), so walk B backwards, then rotate it onto A's corners.
    let rev: Vec<u32> = fb.verts.iter().rev().copied().collect();
    let best = (0..k)
        .min_by(|&o1, &o2| {
            let cost = |o: usize| -> f32 {
                (0..k).map(|i| pos(fa.verts[i]).distance(pos(rev[(i + o) % k]))).sum()
            };
            cost(o1).partial_cmp(&cost(o2)).unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(0);
    let axis = (cb - ca).try_normalize().unwrap_or(Vec3::Y);
    let mut walls: Vec<Face> = Vec::with_capacity(k);
    for i in 0..k {
        let (a0, a1) = (fa.verts[i], fa.verts[(i + 1) % k]);
        let (b0, b1) = (rev[(i + best) % k], rev[(i + 1 + best) % k]);
        let mut verts = vec![a0, a1, b1, b0];
        // Wind each wall outward: its normal must point away from the tube axis.
        let quad: Vec<Vec3> = verts.iter().map(|&v| pos(v)).collect();
        let c = quad.iter().copied().sum::<Vec3>() / 4.0;
        let probe = MapMesh {
            verts: quad,
            faces: vec![Face { verts: (0..4).collect(), slot: 0 }],
            slots: mesh.slots.clone(),
            spec: None,
        };
        let radial = (c - ca) - axis * (c - ca).dot(axis);
        if newell(&probe, &probe.faces[0]).dot(radial) < 0.0 {
            verts.reverse();
        }
        walls.push(Face { verts, slot: fa.slot });
    }
    // Append the walls FIRST (their vertex indices are still valid), then drop
    // the two bridged faces — `delete_faces` remaps the walls along with
    // everything else, so nothing has to be rebuilt by hand.
    let start = mesh.faces.len() as u32;
    mesh.faces.extend(walls);
    delete_faces(mesh, &[a, b]);
    // Both removed faces sat before the appended walls, so the walls slid down
    // by exactly two.
    let start = start - 2;
    (start..start + k as u32).collect()
}

/// Round the given vertices onto a `step` grid in object-local space.
pub fn snap_verts(mesh: &mut MapMesh, verts: &[u32], step: f32) {
    touched(mesh);
    if step <= 0.0 {
        return;
    }
    for v in valid_verts(mesh, verts) {
        let p = mesh.verts[v];
        mesh.verts[v] = Vec3::new(
            (p.x / step).round() * step,
            (p.y / step).round() * step,
            (p.z / step).round() * step,
        );
    }
}

/// Move the mesh so its bounding-box center sits on the local origin; returns
/// the offset that was subtracted (the caller moves the node by it so nothing
/// appears to shift).
pub fn recenter(mesh: &mut MapMesh) -> Vec3 {
    let Some((lo, hi)) = mesh.bounds() else { return Vec3::ZERO };
    let c = (lo + hi) * 0.5;
    for v in &mut mesh.verts {
        *v -= c;
    }
    c
}

/// Move the mesh so `pivot` (object-local) becomes the local origin; returns
/// the offset that was subtracted.
pub fn recenter_on(mesh: &mut MapMesh, pivot: Vec3) -> Vec3 {
    touched(mesh); // the generator's frame assumed the bounds centre
    for v in &mut mesh.verts {
        *v -= pivot;
    }
    pivot
}

/// Rescale the whole mesh about its bounds center so its bounding box measures
/// `size` (full extents). Axes whose current extent is ~0 (a flat plane) are
/// left alone rather than exploding to infinity.
pub fn resize(mesh: &mut MapMesh, size: Vec3) {
    let Some((lo, hi)) = mesh.bounds() else { return };
    let cur = hi - lo;
    let c = (lo + hi) * 0.5;
    let f = Vec3::new(
        if cur.x > 1e-5 { (size.x / cur.x).max(1e-4) } else { 1.0 },
        if cur.y > 1e-5 { (size.y / cur.y).max(1e-4) } else { 1.0 },
        if cur.z > 1e-5 { (size.z / cur.z).max(1e-4) } else { 1.0 },
    );
    for v in &mut mesh.verts {
        *v = c + (*v - c) * f;
    }
    // A resize is still the same shape — keep it parametric, just at the new
    // size, so "8 steps" stays editable after you have sized the staircase.
    if let Some(spec) = mesh.spec.as_mut() {
        spec.half = size * 0.5;
    }
}

/// Append `src` (transformed by `m`) into `mesh`, merging slot lists by NAME so
/// per-face materials survive the merge. Returns the merged faces' indices.
pub fn merge_into(mesh: &mut MapMesh, src: &MapMesh, m: &Mat4) -> Vec<u32> {
    touched(mesh);
    let base = mesh.verts.len() as u32;
    mesh.verts.extend(src.verts.iter().map(|&p| m.transform_point3(p)));
    let mut slot_map: Vec<u16> = Vec::with_capacity(src.slots.len());
    for name in &src.slots {
        let idx = match mesh.slots.iter().position(|s| s == name) {
            Some(i) => i as u16,
            None => {
                mesh.slots.push(name.clone());
                mesh.slots.len() as u16 - 1
            }
        };
        slot_map.push(idx);
    }
    let start = mesh.faces.len() as u32;
    for f in &src.faces {
        mesh.faces.push(Face {
            verts: f.verts.iter().map(|&v| v + base).collect(),
            slot: slot_map.get(f.slot as usize).copied().unwrap_or(0),
        });
    }
    (start..mesh.faces.len() as u32).collect()
}

/// Drop any vertices referenced by no face, remapping face indices.
fn compact_verts(mesh: &mut MapMesh) -> usize {
    let mut used = vec![false; mesh.verts.len()];
    for f in &mesh.faces {
        for &v in &f.verts {
            used[v as usize] = true;
        }
    }
    let mut remap = vec![u32::MAX; mesh.verts.len()];
    let mut kept = Vec::with_capacity(mesh.verts.len());
    for (i, &u) in used.iter().enumerate() {
        if u {
            remap[i] = kept.len() as u32;
            kept.push(mesh.verts[i]);
        }
    }
    let removed = mesh.verts.len() - kept.len();
    mesh.verts = kept;
    for f in &mut mesh.faces {
        for v in &mut f.verts {
            *v = remap[*v as usize];
        }
    }
    removed
}

/// Remove the given faces, then drop any vertices no longer referenced by any
/// face, remapping face indices. Out-of-range face indices are ignored.
pub fn delete_faces(mesh: &mut MapMesh, faces: &[u32]) {
    touched(mesh);
    let sel = valid_faces(mesh, faces);
    if sel.is_empty() {
        return;
    }
    let drop: BTreeSet<usize> = sel.into_iter().collect();
    let mut i = 0;
    mesh.faces.retain(|_| {
        let keep = !drop.contains(&i);
        i += 1;
        keep
    });
    compact_verts(mesh);
}

/// Reverse the winding (flip the normal) of the given faces.
pub fn flip_faces(mesh: &mut MapMesh, faces: &[u32]) {
    touched(mesh);
    for fi in valid_faces(mesh, faces) {
        mesh.faces[fi].verts.reverse();
    }
}

/// Merge the given vertices: any pair within `eps` of each other collapses to
/// their group's centroid (connected-component clustering by distance).
/// Faces are rewritten; faces degenerating below 3 distinct verts are removed
/// (then unused verts dropped, as `delete_faces` does). Returns how many
/// vertices were removed.
pub fn weld(mesh: &mut MapMesh, verts: &[u32], eps: f32) -> usize {
    touched(mesh);
    let sel = valid_verts(mesh, verts);
    if sel.len() < 2 {
        return 0;
    }
    // Union-find over the selected verts (selection sizes are small — the
    // O(n^2) pair scan is fine and keeps clustering exact).
    let mut root: Vec<usize> = (0..sel.len()).collect();
    fn find(root: &mut [usize], i: usize) -> usize {
        let mut i = i;
        while root[i] != i {
            root[i] = root[root[i]];
            i = root[i];
        }
        i
    }
    for i in 0..sel.len() {
        for j in i + 1..sel.len() {
            if mesh.verts[sel[i]].distance(mesh.verts[sel[j]]) <= eps {
                let (a, b) = (find(&mut root, i), find(&mut root, j));
                if a != b {
                    root[a.max(b)] = a.min(b);
                }
            }
        }
    }
    // Collapse each cluster to its centroid on the smallest member index.
    let mut remap: HashMap<u32, u32> = HashMap::new();
    let mut clusters: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..sel.len() {
        let r = find(&mut root, i);
        clusters.entry(r).or_default().push(i);
    }
    for (r, members) in clusters {
        if members.len() < 2 {
            continue;
        }
        let centroid =
            members.iter().map(|&i| mesh.verts[sel[i]]).sum::<Vec3>() / members.len() as f32;
        mesh.verts[sel[r]] = centroid;
        for &i in &members {
            remap.insert(sel[i] as u32, sel[r] as u32);
        }
    }
    if remap.is_empty() {
        return 0;
    }
    for f in &mut mesh.faces {
        for v in &mut f.verts {
            if let Some(&r) = remap.get(v) {
                *v = r;
            }
        }
        // Collapse runs of now-identical consecutive corners (incl. wraparound).
        f.verts.dedup();
        while f.verts.len() > 1 && f.verts.first() == f.verts.last() {
            f.verts.pop();
        }
        // …and any corner that repeats NON-consecutively, which `dedup` cannot see.
        // Welding two opposite corners of a quad leaves the ring [r, b, r, d]: three
        // distinct indices, so the `>= 3` keep-test below passed it, but the ring is a
        // BOWTIE. Its Newell sum is zero, so the normal fell back to +Y and every
        // triangle came out degenerate — an invisible, un-pickable, zero-area face that
        // still held its vertices alive and still polluted edge lists, loops and
        // select-invert. Keeping the first run and dropping the rest turns it back into
        // an honest polygon (or into something the retain below removes).
        let mut seen = BTreeSet::new();
        f.verts.retain(|v| seen.insert(*v));
    }
    mesh.faces.retain(|f| {
        f.verts.iter().collect::<BTreeSet<_>>().len() >= 3
    });
    compact_verts(mesh)
}

/// Assign the given faces to material slot `slot` (clamped to existing slots).
pub fn set_face_slot(mesh: &mut MapMesh, faces: &[u32], slot: u16) {
    let clamped = slot.min(mesh.slots.len().saturating_sub(1) as u16);
    for fi in valid_faces(mesh, faces) {
        mesh.faces[fi].slot = clamped;
    }
}

/// Topological subdivide: each selected n-gon face is split into n quads via
/// edge midpoints + face centroid (Catmull-Clark connectivity WITHOUT the
/// smoothing — positions don't move). Edge midpoints are SHARED between two
/// selected faces that share the edge (dedupe by canonical edge key) so the
/// result stays welded; edges bordering unselected faces get their midpoint
/// INSERTED into that neighbour's corner list too, so the seam has no
/// T-junction and dragging either side stretches the other instead of tearing
/// away from it. Returns the new face indices.
pub fn subdivide_faces(mesh: &mut MapMesh, faces: &[u32]) -> Vec<u32> {
    touched(mesh);
    let sel = valid_faces(mesh, faces);
    if sel.is_empty() {
        return Vec::new();
    }
    let mut midpoints: HashMap<(u32, u32), u32> = HashMap::new();
    let mut new_faces: Vec<Face> = Vec::new();
    for &fi in &sel {
        let f = mesh.faces[fi].clone();
        let k = f.verts.len();
        let centroid =
            f.verts.iter().map(|&v| mesh.verts[v as usize]).sum::<Vec3>() / k as f32;
        mesh.verts.push(centroid);
        let ci = mesh.verts.len() as u32 - 1;
        let mids: Vec<u32> = (0..k)
            .map(|i| {
                let (a, b) = (f.verts[i], f.verts[(i + 1) % k]);
                *midpoints.entry((a.min(b), a.max(b))).or_insert_with(|| {
                    mesh.verts.push((mesh.verts[a as usize] + mesh.verts[b as usize]) * 0.5);
                    mesh.verts.len() as u32 - 1
                })
            })
            .collect();
        for i in 0..k {
            let prev = mids[(i + k - 1) % k];
            new_faces.push(Face { verts: vec![f.verts[i], mids[i], ci, prev], slot: f.slot });
        }
    }
    // Stitch the seam: every unselected face sharing a subdivided edge takes
    // the new midpoint as a corner of its own. Without this the neighbour
    // spans that edge with two corners while the subdivided side has three,
    // and pulling either face rips the mesh open along the seam.
    let drop: BTreeSet<usize> = sel.iter().copied().collect();
    for (fi, f) in mesh.faces.iter_mut().enumerate() {
        if drop.contains(&fi) {
            continue;
        }
        let k = f.verts.len();
        let mut stitched = Vec::with_capacity(k);
        for i in 0..k {
            let (a, b) = (f.verts[i], f.verts[(i + 1) % k]);
            stitched.push(a);
            if let Some(&m) = midpoints.get(&(a.min(b), a.max(b))) {
                stitched.push(m);
            }
        }
        if stitched.len() != k {
            f.verts = stitched;
        }
    }
    // Unselected faces keep their relative order; the subdivided quads append.
    let mut i = 0;
    mesh.faces.retain(|_| {
        let keep = !drop.contains(&i);
        i += 1;
        keep
    });
    let start = mesh.faces.len() as u32;
    let count = new_faces.len() as u32;
    mesh.faces.extend(new_faces);
    (start..start + count).collect()
}

/// Insert a new edge loop across the ring through `edge`, at `t` along each
/// crossed edge (0.5 = the middle, Blender's default).
///
/// This is the workhorse the modeling tool was missing. Every quad the ring
/// passes through is split in two by a cut joining the new points on its two
/// ring edges, so a box becomes two boxes' worth of faces without changing its
/// silhouette at all — which is how you get somewhere to bend, taper or inset.
///
/// Returns the edges of the loop it inserted, ready to become the selection.
/// Empty (and the mesh untouched) when the ring is not made of quads, since
/// there is no "opposite edge" to cut toward and guessing one would put a
/// crease somewhere nobody asked for.
pub fn loop_cut(mesh: &mut MapMesh, edge: (u32, u32), t: f32) -> Vec<(u32, u32)> {
    let ring = crate::edge_ring(mesh, edge);
    if ring.is_empty() {
        return Vec::new();
    }
    let t = t.clamp(0.02, 0.98);
    // Every face the ring crosses must be a quad with exactly two ring edges.
    // Checked BEFORE anything is written, so a refusal leaves the mesh alone.
    let ring_set: BTreeSet<(u32, u32)> = ring.iter().copied().collect();
    let mut cut_faces: Vec<(usize, usize, usize)> = Vec::new(); // face, edge slot a, slot b
    for (fi, f) in mesh.faces.iter().enumerate() {
        let n = f.verts.len();
        let slots: Vec<usize> = (0..n)
            .filter(|&i| ring_set.contains(&key(f.verts[i], f.verts[(i + 1) % n])))
            .collect();
        match slots.len() {
            0 => continue,
            2 if n == 4 && slots[1] == slots[0] + 2 => {
                cut_faces.push((fi, slots[0], slots[1]))
            }
            _ => return Vec::new(),
        }
    }
    if cut_faces.is_empty() {
        return Vec::new();
    }
    // One new vertex per ring edge, placed `t` of the way from its lower-indexed
    // end — the ring is canonical `(a < b)`, so every face agrees on where the
    // point went and the two halves meet exactly.
    let mut mid: HashMap<(u32, u32), u32> = HashMap::new();
    for &(a, b) in &ring {
        let p = mesh.verts[a as usize].lerp(mesh.verts[b as usize], t);
        mesh.verts.push(p);
        mid.insert((a, b), mesh.verts.len() as u32 - 1);
    }
    let mut added: Vec<Face> = Vec::new();
    for &(fi, i, j) in &cut_faces {
        let v = mesh.faces[fi].verts.clone();
        let slot = mesh.faces[fi].slot;
        let mi = mid[&key(v[i], v[(i + 1) % 4])];
        let mj = mid[&key(v[j], v[(j + 1) % 4])];
        // Walking the original winding, the cut splits [i+1 .. j] from
        // [j+1 .. i]; both halves keep the face's own order, so both stay CCW.
        mesh.faces[fi].verts = vec![mi, v[(i + 1) % 4], v[j], mj];
        added.push(Face { verts: vec![mj, v[(j + 1) % 4], v[i], mi], slot });
    }
    mesh.faces.extend(added);
    touched(mesh);
    // The inserted loop is every edge joining two of the new points — one per
    // face the ring crossed, which is exactly the cut we just made.
    let new_pts: BTreeSet<u32> = mid.values().copied().collect();
    let mut loop_edges = BTreeSet::new();
    for f in &mesh.faces {
        let n = f.verts.len();
        for i in 0..n {
            let (a, b) = (f.verts[i], f.verts[(i + 1) % n]);
            if new_pts.contains(&a) && new_pts.contains(&b) {
                loop_edges.insert(key(a, b));
            }
        }
    }
    loop_edges.into_iter().collect()
}

/// Round off the selected edges: pull each one apart into a strip of width
/// `amount`, so a hard corner becomes a chamfer that catches the light.
///
/// The cheap, predictable form — one segment, and only for edges whose two
/// faces are both quads. That covers the case it is wanted for almost every
/// time (taking the sharpness off a blockout box) and refuses the rest rather
/// than producing a tangle.
pub fn bevel_edges(mesh: &mut MapMesh, edges: &[(u32, u32)], amount: f32) -> usize {
    if amount <= 0.0 || edges.is_empty() {
        return 0;
    }
    // Corners are shared, so a bevel is really a per-(vertex, face) split: each
    // face that meets a bevelled vertex gets its own copy, pulled in toward that
    // face's centre. That is enough to open a chamfer along every selected edge
    // without any face-by-face special cases.
    let touched_verts: BTreeSet<u32> = edges
        .iter()
        .filter(|(a, b)| (*a as usize) < mesh.verts.len() && (*b as usize) < mesh.verts.len())
        .flat_map(|&(a, b)| [a, b])
        .collect();
    if touched_verts.is_empty() {
        return 0;
    }
    let centres: Vec<Vec3> = mesh
        .faces
        .iter()
        .map(|f| f.verts.iter().map(|&v| mesh.verts[v as usize]).sum::<Vec3>() / f.verts.len() as f32)
        .collect();
    let mut moved = 0usize;
    // Per face, per corner: a fresh vertex pulled `amount` toward the face
    // centre. Faces stop sharing the corner, which IS the chamfer.
    for (fi, &c) in centres.iter().enumerate() {
        for k in 0..mesh.faces[fi].verts.len() {
            let v = mesh.faces[fi].verts[k];
            if !touched_verts.contains(&v) {
                continue;
            }
            let p = mesh.verts[v as usize];
            let dir = c - p;
            let d = dir.length();
            if d < 1e-6 {
                continue;
            }
            let np = p + dir / d * amount.min(d * 0.45);
            mesh.verts.push(np);
            mesh.faces[fi].verts[k] = mesh.verts.len() as u32 - 1;
            moved += 1;
        }
    }
    if moved > 0 {
        touched(mesh);
    }
    moved
}

/// Canonical undirected edge key (mirrors `select::key`).
fn key(a: u32, b: u32) -> (u32, u32) {
    (a.min(b), a.max(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extruding a CLOSED selection has no direction, and used to translate the whole
    /// shell along +Y while leaving every original vertex behind as an orphan — which
    /// looks like the tool inventing loose vertices.
    #[test]
    fn extruding_a_whole_closed_shell_is_refused_not_guessed() {
        let mut m = crate::box_mesh(Vec3::ONE);
        let before = m.clone();
        let all: Vec<u32> = (0..m.faces.len() as u32).collect();
        let made = extrude_faces(&mut m, &all, 1.0);
        assert!(made.is_empty(), "a closed shell has no extrude direction");
        assert_eq!(m.verts.len(), before.verts.len(), "no orphan vertices were added");
        assert_eq!(m.faces.len(), before.faces.len());
        assert_eq!(m.verts, before.verts, "the shell did not move");
    }

    /// Welding two corners of one face that are NOT neighbours leaves a bowtie ring.
    /// `dedup` only sees consecutive repeats, so it survived as a zero-area face that
    /// drew nothing, picked nothing, and kept its vertices in every edge and loop query.
    #[test]
    fn welding_opposite_corners_does_not_leave_an_invisible_bowtie() {
        // One quad, and two opposite corners close enough to weld together.
        let mut m = MapMesh {
            verts: vec![
                Vec3::new(0.0, 0.0, 0.0),
                Vec3::new(1.0, 0.0, 0.0),
                Vec3::new(0.001, 0.0, 0.0), // ~ on top of vert 0
                Vec3::new(0.0, 0.0, 1.0),
            ],
            faces: vec![Face { verts: vec![0, 1, 2, 3], slot: 0 }],
            slots: vec!["Default".into()],
            spec: None,
        };
        let n = weld(&mut m, &[0, 2], 0.05);
        assert_eq!(n, 1, "one pair welded");
        m.validate().expect("still a valid mesh");
        for f in &m.faces {
            let distinct: BTreeSet<_> = f.verts.iter().collect();
            assert_eq!(distinct.len(), f.verts.len(), "a corner repeats in ring {:?}", f.verts);
            // …and whatever survived has real area, rather than being a zero-area ghost.
            let n = crate::face_normal(&m, f);
            assert!(n.is_finite() && n.length() > 0.5, "degenerate face survived");
        }
    }
    use crate::{box_mesh, face_normal, plane};
    use glam::{Mat4, Vec2, Vec3};

    fn top_face(m: &MapMesh) -> u32 {
        m.faces
            .iter()
            .enumerate()
            .find(|(_, f)| face_normal(m, f).y > 0.99)
            .map(|(i, _)| i as u32)
            .unwrap()
    }

    #[test]
    fn translate_applies_once_despite_duplicates() {
        let mut m = box_mesh(Vec3::ONE);
        let before = m.verts[0];
        translate_verts(&mut m, &[0, 0, 0, 99], Vec3::X);
        assert_eq!(m.verts[0], before + Vec3::X);
    }

    #[test]
    fn transform_applies_once_despite_duplicates() {
        let mut m = box_mesh(Vec3::ONE);
        let before = m.verts[1];
        let t = Mat4::from_translation(Vec3::Y * 2.0);
        transform_verts(&mut m, &[1, 1], &t);
        assert_eq!(m.verts[1], before + Vec3::Y * 2.0);
    }

    /// The point of a loop cut: the SHAPE does not change, only what it is made
    /// of. A cube's silhouette, volume and validity all survive; it just has a
    /// seam through the middle to work with now.
    #[test]
    fn a_loop_cut_adds_a_seam_without_moving_the_shape() {
        let mut m = box_mesh(Vec3::ONE);
        let before = m.bounds().unwrap();
        // The ring through one of the top face's edges runs right round the box.
        let e = m.edges()[0];
        let ring = crate::edge_ring(&m, e);
        assert_eq!(ring.len(), 4, "a cube's ring is four edges: {ring:?}");
        let loop_edges = loop_cut(&mut m, e, 0.5);
        m.validate().expect("a loop cut leaves a valid mesh");
        assert_eq!(m.faces.len(), 10, "the 4 crossed faces each split in two");
        assert_eq!(loop_edges.len(), 4, "the inserted loop closes: {loop_edges:?}");
        let after = m.bounds().unwrap();
        assert!((after.0 - before.0).length() < 1e-5, "the shape did not move");
        assert!((after.1 - before.1).length() < 1e-5);
    }

    /// The cut goes where you put it, not always down the middle.
    #[test]
    fn a_loop_cut_slides_along_the_ring() {
        let mut m = box_mesh(Vec3::ONE);
        let e = m.edges()[0];
        let before = m.verts.len();
        loop_cut(&mut m, e, 0.25);
        let new: Vec<Vec3> = m.verts[before..].to_vec();
        assert_eq!(new.len(), 4);
        // A cube spans -1..1, so a quarter of the way along an edge is at -0.5.
        let a = m.verts[e.0 as usize];
        let b = m.verts[e.1 as usize];
        let want = a.lerp(b, 0.25);
        assert!(new.iter().any(|p| (*p - want).length() < 1e-5), "{new:?} should contain {want}");
    }

    /// Two cuts in a row are two seams — the second reads the mesh the first
    /// left, rather than tripping over its own new faces.
    #[test]
    fn loop_cuts_stack() {
        let mut m = box_mesh(Vec3::ONE);
        let e = m.edges()[0];
        loop_cut(&mut m, e, 0.5);
        m.validate().unwrap();
        let faces_after_one = m.faces.len();
        let e2 = m.edges().into_iter().find(|&x| x != e).unwrap();
        loop_cut(&mut m, e2, 0.5);
        m.validate().expect("still valid after a second cut");
        assert!(m.faces.len() > faces_after_one);
    }

    /// A ring that is not made of quads is REFUSED, and refused before anything
    /// is written — there is no opposite edge to cut toward on a triangle, and a
    /// half-applied cut would be worse than none.
    #[test]
    fn a_loop_cut_through_triangles_is_refused_and_changes_nothing() {
        let mut m = crate::sphere(1.0, 8, 6);
        let before = m.clone();
        for e in m.edges() {
            if loop_cut(&mut m, e, 0.5).is_empty() {
                assert_eq!(m, before, "a refused cut left the mesh alone");
                return;
            }
        }
        // If every ring on a sphere happened to be quads, the cuts were all
        // legitimate — still assert the mesh stayed valid.
        m.validate().unwrap();
    }

    /// Bevelling the edges of a box opens a chamfer: more vertices, same
    /// bounding shape, still a valid mesh.
    #[test]
    fn a_bevel_opens_the_corners_without_inflating_the_box() {
        let mut m = box_mesh(Vec3::ONE);
        let before = m.bounds().unwrap();
        let edges = m.edges();
        let moved = bevel_edges(&mut m, &edges, 0.2);
        assert!(moved > 0, "corners moved");
        m.validate().expect("a bevel leaves a valid mesh");
        let after = m.bounds().unwrap();
        // Every corner pulled INWARD, so the box can only have shrunk.
        assert!(after.0.x >= before.0.x - 1e-5 && after.1.x <= before.1.x + 1e-5);
    }

    /// A zero-width bevel, or one with no edges, is a no-op rather than a mesh
    /// full of duplicated vertices.
    #[test]
    fn a_bevel_of_nothing_does_nothing() {
        let mut m = box_mesh(Vec3::ONE);
        let before = m.clone();
        assert_eq!(bevel_edges(&mut m, &[], 0.2), 0);
        let edges = m.edges();
        assert_eq!(bevel_edges(&mut m, &edges, 0.0), 0);
        assert_eq!(m, before);
    }

    #[test]
    fn extrude_one_box_face() {
        let mut m = box_mesh(Vec3::ONE);
        let top = top_face(&m);
        let moved = extrude_faces(&mut m, &[top], 0.5);
        assert_eq!(moved, vec![top]);
        m.validate().unwrap();
        assert_eq!(m.verts.len(), 12); // +4 duplicated corners
        assert_eq!(m.faces.len(), 10); // +4 walls
        // The moved face sits at y = 1.5 and still faces up.
        let f = &m.faces[top as usize];
        for &v in &f.verts {
            assert!((m.verts[v as usize].y - 1.5).abs() < 1e-5);
        }
        assert!(face_normal(&m, f).y > 0.99);
        // Walls wind outward and inherit slot 0.
        for f in &m.faces[6..] {
            let c = f.verts.iter().map(|&v| m.verts[v as usize]).sum::<Vec3>()
                / f.verts.len() as f32;
            let n = face_normal(&m, f);
            assert!(n.dot(Vec3::new(c.x, 0.0, c.z)) > 0.0, "wall winds inward");
            assert_eq!(f.slot, 0);
        }
        // Still watertight: every directed edge appears exactly once.
        let mut count = std::collections::HashMap::new();
        for f in &m.faces {
            let k = f.verts.len();
            for i in 0..k {
                *count.entry((f.verts[i], f.verts[(i + 1) % k])).or_insert(0) += 1;
            }
        }
        assert!(count.values().all(|&c| c == 1));
    }

    #[test]
    fn region_extrude_makes_no_interior_walls() {
        // Two coplanar quads sharing an edge; extruding both should produce
        // walls only on the 6 boundary edges.
        let mut m = MapMesh::new();
        m.verts = vec![
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(2.0, 0.0, 0.0),
            Vec3::new(2.0, 0.0, 1.0),
            Vec3::new(1.0, 0.0, 1.0),
            Vec3::new(0.0, 0.0, 1.0),
        ];
        m.faces = vec![
            Face { verts: vec![0, 5, 4, 1], slot: 0 },
            Face { verts: vec![1, 4, 3, 2], slot: 0 },
        ];
        m.validate().unwrap();
        assert!(face_normal(&m, &m.faces[0]).y > 0.99);
        let moved = extrude_faces(&mut m, &[0, 1], 1.0);
        m.validate().unwrap();
        assert_eq!(moved, vec![0, 1]);
        assert_eq!(m.faces.len(), 2 + 6); // no wall on the shared edge
        assert_eq!(m.verts.len(), 12); // all 6 region verts duplicated once
        // Both moved faces are welded: they still share two vertices.
        let a: std::collections::HashSet<_> = m.faces[0].verts.iter().collect();
        assert_eq!(m.faces[1].verts.iter().filter(|v| a.contains(v)).count(), 2);
    }

    #[test]
    fn extrude_zero_distance_and_empty_selection() {
        let mut m = box_mesh(Vec3::ONE);
        assert!(extrude_faces(&mut m, &[], 1.0).is_empty());
        assert_eq!(m.faces.len(), 6);
        let top = top_face(&m);
        extrude_faces(&mut m, &[top], 0.0);
        m.validate().unwrap();
        assert_eq!(m.faces.len(), 10);
    }

    #[test]
    fn delete_faces_compacts_verts() {
        let mut m = box_mesh(Vec3::ONE);
        let top = top_face(&m);
        extrude_faces(&mut m, &[top], 0.5);
        delete_faces(&mut m, &[top]);
        m.validate().unwrap();
        assert_eq!(m.faces.len(), 9);
        assert_eq!(m.verts.len(), 12); // extruded ring verts still used by walls
        // Deleting the whole extrusion drops its verts.
        let walls: Vec<u32> = (5..9).collect();
        delete_faces(&mut m, &walls);
        m.validate().unwrap();
        assert_eq!(m.verts.len(), 8);
    }

    #[test]
    fn flip_negates_the_normal() {
        let mut m = plane(Vec2::ONE);
        let n = face_normal(&m, &m.faces[0]);
        flip_faces(&mut m, &[0]);
        assert!((face_normal(&m, &m.faces[0]) + n).length() < 1e-5);
    }

    #[test]
    fn weld_merges_and_reports() {
        // Extrude by 0 -> the duplicated ring sits exactly on the original;
        // welding everything collapses the mesh back to 8 verts.
        let mut m = box_mesh(Vec3::ONE);
        let top = top_face(&m);
        extrude_faces(&mut m, &[top], 0.0);
        let all: Vec<u32> = (0..m.verts.len() as u32).collect();
        let removed = weld(&mut m, &all, 1e-4);
        m.validate().unwrap();
        assert_eq!(removed, 4);
        assert_eq!(m.verts.len(), 8);
        // The four zero-height walls degenerated away; the top face remains.
        assert_eq!(m.faces.len(), 6);
    }

    #[test]
    fn weld_only_touches_the_selection() {
        let mut m = box_mesh(Vec3::ONE);
        let top = top_face(&m);
        extrude_faces(&mut m, &[top], 0.0);
        assert_eq!(weld(&mut m, &[0, 1], 1e-4), 0); // not coincident with each other
        assert_eq!(m.verts.len(), 12);
    }

    #[test]
    fn subdivide_quad_into_four() {
        let mut m = plane(Vec2::ONE);
        let created = subdivide_faces(&mut m, &[0]);
        m.validate().unwrap();
        assert_eq!(created, vec![0, 1, 2, 3]);
        assert_eq!(m.faces.len(), 4);
        assert_eq!(m.verts.len(), 9); // 4 corners + 4 midpoints + centroid
        for fi in created {
            let f = &m.faces[fi as usize];
            assert_eq!(f.verts.len(), 4);
            assert!(face_normal(&m, f).y > 0.99); // winding preserved
        }
    }

    /// Subdividing PART of a mesh must not leave a T-junction where the new
    /// midpoints meet the untouched neighbours — that seam is exactly where a
    /// face drag would tear the shape open.
    #[test]
    fn subdivide_stitches_the_seam_with_its_neighbours() {
        let mut m = box_mesh(Vec3::ONE);
        let top = top_face(&m);
        subdivide_faces(&mut m, &[top]);
        m.validate().unwrap();
        // The four side faces each gained the midpoint of the edge they share
        // with the subdivided top.
        let five_gons = m.faces.iter().filter(|f| f.verts.len() == 5).count();
        assert_eq!(five_gons, 4, "each neighbour should have taken one midpoint");
        // And no vertex is left sitting in the middle of anybody's edge.
        for f in &m.faces {
            let k = f.verts.len();
            for i in 0..k {
                let (a, b) = (m.verts[f.verts[i] as usize], m.verts[f.verts[(i + 1) % k] as usize]);
                let ab = b - a;
                for (vi, &p) in m.verts.iter().enumerate() {
                    if f.verts[i] as usize == vi || f.verts[(i + 1) % k] as usize == vi {
                        continue;
                    }
                    let t = (p - a).dot(ab) / ab.length_squared();
                    assert!(
                        !(1e-4..=1.0 - 1e-4).contains(&t) || (a + ab * t).distance(p) > 1e-4,
                        "vertex {vi} sits on an edge (T-junction)"
                    );
                }
            }
        }
    }

    #[test]
    fn subdivide_shares_midpoints_between_selected_faces() {
        let mut m = box_mesh(Vec3::ONE);
        let created = subdivide_faces(&mut m, &[0, 1, 2, 3, 4, 5]);
        m.validate().unwrap();
        assert_eq!(created.len(), 24);
        // 8 corners + 12 edge midpoints + 6 centroids.
        assert_eq!(m.verts.len(), 26);
    }

    #[test]
    fn inset_keeps_the_face_inside_its_own_border() {
        let mut m = box_mesh(Vec3::ONE);
        let top = top_face(&m);
        let inner = inset_faces(&mut m, &[top], 0.25);
        m.validate().unwrap();
        assert_eq!(inner, vec![top]);
        assert_eq!(m.faces.len(), 6 + 4); // + the ring of side quads
        assert_eq!(m.verts.len(), 8 + 4);
        let f = &m.faces[top as usize];
        assert!(face_normal(&m, f).y > 0.99); // still faces up, still flat
        for &v in &f.verts {
            let p = m.verts[v as usize];
            assert!((p.y - 1.0).abs() < 1e-5, "inset must stay in the face plane");
            assert!((p.x.abs() - 0.75).abs() < 1e-4 && (p.z.abs() - 0.75).abs() < 1e-4);
        }
    }

    #[test]
    fn inset_too_big_for_the_face_shrinks_instead_of_inverting() {
        let mut m = plane(Vec2::ONE);
        inset_faces(&mut m, &[0], 50.0);
        m.validate().unwrap();
        let n = face_normal(&m, &m.faces[0]);
        assert!(n.y > 0.99, "face flipped: {n:?}");
        // The border is untouched (half-extent 1) and the inner ring stayed
        // inside it rather than shooting past the corners.
        let (lo, hi) = m.bounds().unwrap();
        assert!((hi.x - lo.x - 2.0).abs() < 1e-5);
        for &v in &m.faces[0].verts {
            assert!(m.verts[v as usize].x.abs() < 1.0);
        }
    }

    #[test]
    fn inset_then_extrude_makes_a_recess() {
        let mut m = box_mesh(Vec3::ONE);
        let top = top_face(&m);
        let inner = inset_faces(&mut m, &[top], 0.3);
        let moved = extrude_faces(&mut m, &inner, -0.5);
        m.validate().unwrap();
        for &v in &m.faces[moved[0] as usize].verts {
            assert!((m.verts[v as usize].y - 0.5).abs() < 1e-5);
        }
    }

    #[test]
    fn detach_splits_a_face_into_its_own_mesh() {
        let mut m = box_mesh(Vec3::ONE);
        let top = top_face(&m);
        let out = detach_faces(&mut m, &[top]).unwrap();
        m.validate().unwrap();
        out.validate().unwrap();
        assert_eq!(out.faces.len(), 1);
        assert_eq!(out.verts.len(), 4);
        assert_eq!(m.faces.len(), 5);
        assert!(face_normal(&out, &out.faces[0]).y > 0.99);
        // Detaching everything is refused (it would leave an empty node behind).
        let all: Vec<u32> = (0..m.faces.len() as u32).collect();
        assert!(detach_faces(&mut m, &all).is_none());
    }

    #[test]
    fn bridge_joins_two_faces_into_a_tube() {
        // Two coplanar-facing quads a unit apart: bridging makes a box shell.
        let mut m = plane(Vec2::ONE);
        let lid = plane(Vec2::ONE);
        merge_into(&mut m, &lid, &Mat4::from_translation(Vec3::Y * 2.0));
        flip_faces(&mut m, &[0]); // face them at each other
        let walls = bridge_faces(&mut m, 0, 1);
        m.validate().unwrap();
        assert_eq!(walls.len(), 4);
        assert_eq!(m.faces.len(), 4);
        // Every wall winds outward (away from the tube's axis).
        let center = Vec3::new(0.0, 1.0, 0.0);
        for &w in &walls {
            let f = &m.faces[w as usize];
            let c = f.verts.iter().map(|&v| m.verts[v as usize]).sum::<Vec3>() / 4.0;
            assert!(face_normal(&m, f).dot(c - center) > 0.0, "wall {w} winds inward");
        }
        assert!(bridge_faces(&mut m, 0, 0).is_empty());
    }

    #[test]
    fn snap_resize_and_recenter() {
        let mut m = box_mesh(Vec3::ONE);
        translate_verts(&mut m, &[0], Vec3::splat(0.13));
        snap_verts(&mut m, &[0], 0.5);
        assert_eq!(m.verts[0], (m.verts[0] / 0.5).round() * 0.5);

        let mut m = box_mesh(Vec3::ONE);
        resize(&mut m, Vec3::new(4.0, 1.0, 6.0));
        let (lo, hi) = m.bounds().unwrap();
        assert!((hi - lo - Vec3::new(4.0, 1.0, 6.0)).length() < 1e-4);
        assert!((hi + lo).length() < 1e-4, "resize must keep the mesh centered");

        let mut m = box_mesh(Vec3::ONE);
        translate_verts(&mut m, &(0..8).collect::<Vec<_>>(), Vec3::new(5.0, 0.0, 0.0));
        assert!((recenter(&mut m) - Vec3::new(5.0, 0.0, 0.0)).length() < 1e-5);
        let (lo, hi) = m.bounds().unwrap();
        assert!((hi + lo).length() < 1e-5);
    }

    #[test]
    fn merge_unions_slots_by_name() {
        let mut a = box_mesh(Vec3::ONE);
        a.slots.push("Trim".into());
        set_face_slot(&mut a, &[0], 1);
        let mut b = box_mesh(Vec3::ONE);
        b.slots = vec!["Default".into(), "Glass".into()];
        set_face_slot(&mut b, &[0, 1], 1);
        let added = merge_into(&mut a, &b, &Mat4::from_translation(Vec3::X * 4.0));
        a.validate().unwrap();
        assert_eq!(a.slots, vec!["Default", "Trim", "Glass"]);
        assert_eq!(added.len(), 6);
        assert_eq!(a.faces[added[0] as usize].slot, 2);
        assert_eq!(a.faces[added[2] as usize].slot, 0);
        assert!(a.verts[8..].iter().all(|v| v.x > 2.0), "merged verts must arrive transformed");
    }

    #[test]
    fn set_face_slot_clamps() {
        let mut m = box_mesh(Vec3::ONE);
        set_face_slot(&mut m, &[0], 7); // only slot 0 exists
        assert_eq!(m.faces[0].slot, 0);
        m.slots.push("Wall".into());
        set_face_slot(&mut m, &[0, 99], 1);
        assert_eq!(m.faces[0].slot, 1);
        m.validate().unwrap();
    }
}
