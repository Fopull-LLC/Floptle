//! Convex hulls: the shape a moving body takes from a mesh.
//!
//! A mesh made in the engine or imported is a list of triangles. A body that
//! falls, bounces and tumbles needs a solid it can turn, weigh and test points
//! against, and a convex hull is the tightest such solid around the triangles:
//! a box hulls to itself, a ramp to its wedge, a chair to the block around it.
//! A static body keeps the exact triangles instead (see [`BodyGeometry`]).
//!
//! The hull is kept small and inline so a [`crate::Body`] stays a plain value
//! the solver can copy: at most [`HULL_VERTS`] corners and [`HULL_PLANES`]
//! faces. A mesh with more corners than that is thinned to the ones that spread
//! furthest, which keeps its extremes and gives up detail between them.

use floptle_core::math::Vec3;

/// The most corners a hull keeps.
pub const HULL_VERTS: usize = 20;
/// The most faces a hull keeps: a triangulated hull of `HULL_VERTS` corners
/// has at most `2·HULL_VERTS − 4`.
pub const HULL_PLANES: usize = 2 * HULL_VERTS - 4;

/// A node's own geometry, for a Rigidbody whose shape is **Mesh**: triangles
/// in the node's local frame, before its scale.
///
/// The editor attaches it at Play from whatever the node draws (a blockout
/// mesh, an imported model, a primitive), because only the editor can read
/// those. The sim reads it when it builds the body: a Dynamic or Kinematic
/// body becomes the [`Hull`] of the points, a Static one bakes the exact
/// triangles. Never saved; it is rebuilt from the mesh every Play, so an edit
/// to the mesh is an edit to its collision.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BodyGeometry {
    pub verts: Vec<Vec3>,
    pub indices: Vec<u32>,
}

/// A convex solid: its corners, its face planes, and its mass properties.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hull {
    verts: [Vec3; HULL_VERTS],
    /// Per corner, the direction that moves it one unit in from every face
    /// it lies on: a corner moved `s` along it, with a sphere of radius `s`,
    /// touches those faces exactly where the corner did.
    inset: [Vec3; HULL_VERTS],
    nv: u8,
    normals: [Vec3; HULL_PLANES],
    offsets: [f32; HULL_PLANES],
    np: u8,
    centroid: Vec3,
    volume: f32,
    /// Moments of inertia per unit mass about the centroid, along the
    /// hull's own axes.
    inertia: Vec3,
}

impl Hull {
    /// The hull of `points`, or `None` when there are none. Points that all
    /// lie in a plane (a quad, a flat sheet) are given a centimetre of
    /// thickness either side so the result is still a solid.
    pub fn from_points(points: &[Vec3]) -> Option<Hull> {
        let pts: Vec<Vec3> = points.iter().copied().filter(|p| p.is_finite()).collect();
        if pts.is_empty() {
            return None;
        }
        let (lo, hi) = bounds(&pts);
        let extent = (hi - lo).max_element().max(1e-4);
        let eps = extent * 1e-5;
        // Far more points than a hull keeps: its corners are always among
        // the furthest points in some direction, so ask a spread of
        // directions first and work with only those.
        let mut cand = if pts.len() > 96 { support_points(&pts) } else { dedup(&pts, eps) };
        thicken(&mut cand, extent);
        let mut faces = hull_faces(&cand, eps * 10.0)?;
        let mut used = face_verts(&faces);
        if used.len() > HULL_VERTS {
            let keep = spread(&cand, &used, HULL_VERTS);
            cand = keep;
            faces = hull_faces(&cand, eps * 10.0)?;
            used = face_verts(&faces);
        }
        let mut h = Hull {
            verts: [Vec3::ZERO; HULL_VERTS],
            inset: [Vec3::ZERO; HULL_VERTS],
            nv: 0,
            normals: [Vec3::Y; HULL_PLANES],
            offsets: [0.0; HULL_PLANES],
            np: 0,
            centroid: Vec3::ZERO,
            volume: 0.0,
            inertia: Vec3::ZERO,
        };
        for &i in used.iter().take(HULL_VERTS) {
            h.verts[h.nv as usize] = cand[i];
            h.nv += 1;
        }
        // One plane per flat face: the triangles of a box's side share it.
        for f in &faces {
            let (a, b, c) = (cand[f[0]], cand[f[1]], cand[f[2]]);
            let Some(n) = (b - a).cross(c - a).try_normalize() else { continue };
            let o = n.dot(a);
            let np = h.np as usize;
            if (0..np).any(|k| h.normals[k].dot(n) > 0.9999 && (h.offsets[k] - o).abs() < eps * 100.0) {
                continue;
            }
            if np < HULL_PLANES {
                h.normals[np] = n;
                h.offsets[np] = o;
                h.np += 1;
            }
        }
        // Each corner's way in: least squares over the faces it lies on, so
        // n · d = 1 for each. Capped, so a needle-sharp corner is rounded a
        // little more rather than pulled through the solid.
        for i in 0..h.nv as usize {
            let v = h.verts[i];
            let (mut a, mut b) = (floptle_core::math::Mat3::ZERO, Vec3::ZERO);
            for k in 0..h.np as usize {
                let n = h.normals[k];
                if (n.dot(v) - h.offsets[k]).abs() < extent * 1e-3 {
                    a += floptle_core::math::Mat3::from_cols(n * n.x, n * n.y, n * n.z);
                    b += n;
                }
            }
            let d = if a.determinant().abs() > 1e-6 { a.inverse() * b } else { b.normalize_or_zero() };
            h.inset[i] = if d.is_finite() { d.clamp_length_max(3.0) } else { Vec3::ZERO };
        }
        // Mass properties of the solid, by tetrahedra from an inside point.
        let r = h.verts[..h.nv as usize].iter().fold(Vec3::ZERO, |s, v| s + *v) / h.nv.max(1) as f32;
        let mut vol = 0.0f32;
        let mut first = Vec3::ZERO;
        let mut second = [[0.0f32; 3]; 3];
        for f in &faces {
            let (a, b, c) = (cand[f[0]] - r, cand[f[1]] - r, cand[f[2]] - r);
            let v = a.dot(b.cross(c)) / 6.0;
            vol += v;
            first += (a + b + c) * (v / 4.0);
            let s = a + b + c;
            let (pa, pb, pc, ps) = (a.to_array(), b.to_array(), c.to_array(), s.to_array());
            for (i, row) in second.iter_mut().enumerate() {
                for (j, x) in row.iter_mut().enumerate() {
                    *x += v / 20.0 * (pa[i] * pa[j] + pb[i] * pb[j] + pc[i] * pc[j] + ps[i] * ps[j]);
                }
            }
        }
        let vol = vol.max(1e-9);
        let c = first / vol;
        let ca = c.to_array();
        let m = |i: usize, j: usize| second[i][j] / vol - ca[i] * ca[j];
        h.centroid = r + c;
        h.volume = vol;
        h.inertia = Vec3::new(m(1, 1) + m(2, 2), m(0, 0) + m(2, 2), m(0, 0) + m(1, 1)).max(Vec3::splat(1e-8));
        Some(h)
    }

    /// The hull's corners.
    pub fn verts(&self) -> &[Vec3] {
        &self.verts[..self.nv as usize]
    }

    /// Corner `i` moved `skin` in from every face it lies on.
    pub(crate) fn inset_vert(&self, i: usize, skin: f32) -> Vec3 {
        self.verts[i] - self.inset[i] * skin
    }

    /// The face planes, as `(outward normal, offset)` with `n · p = offset`
    /// on the face.
    pub fn planes(&self) -> impl Iterator<Item = (Vec3, f32)> + '_ {
        (0..self.np as usize).map(|i| (self.normals[i], self.offsets[i]))
    }

    /// The hull's edges, as pairs of corner indices: two corners that share
    /// two faces. What an outline of the solid draws.
    pub fn edges(&self) -> Vec<(usize, usize)> {
        let n = self.nv as usize;
        let on = |i: usize, k: usize| (self.normals[k].dot(self.verts[i]) - self.offsets[k]).abs() < 1e-4 * (1.0 + self.offsets[k].abs());
        let mut out = Vec::new();
        for a in 0..n {
            for b in a + 1..n {
                if (0..self.np as usize).filter(|&k| on(a, k) && on(b, k)).count() >= 2 {
                    out.push((a, b));
                }
            }
        }
        out
    }

    /// The middle of the solid's volume.
    pub fn centroid(&self) -> Vec3 {
        self.centroid
    }

    /// The solid's volume, cubic units.
    pub fn volume(&self) -> f32 {
        self.volume
    }

    /// Moments of inertia about the centroid for `mass`, along the hull's
    /// own axes. The products of inertia are left out, which is exact for a
    /// box, a prism or anything else symmetric about those axes.
    pub fn inertia(&self, mass: f32) -> Vec3 {
        self.inertia * mass
    }

    /// Signed distance from `p`: exact inside, and outside the largest
    /// distance to a face plane, which never overstates the gap.
    pub fn distance(&self, p: Vec3) -> f32 {
        let mut d = f32::MIN;
        for i in 0..self.np as usize {
            d = d.max(self.normals[i].dot(p) - self.offsets[i]);
        }
        d
    }

    /// The axis-aligned box around the corners, as `(min, max)`.
    pub fn bounds(&self) -> (Vec3, Vec3) {
        bounds(self.verts())
    }

    /// The furthest any corner is from `p`.
    pub fn reach_from(&self, p: Vec3) -> f32 {
        self.verts().iter().map(|v| v.distance(p)).fold(0.0f32, f32::max)
    }

    /// How far the nearest face is from the centroid: the thickness a move
    /// is measured against before it is cut into substeps.
    pub fn inner_radius(&self) -> f32 {
        -self.distance(self.centroid)
    }

    /// Every corner scaled by `s` about the node's origin (a negative scale
    /// mirrors). Rebuilt rather than scaled in place, because a non-uniform
    /// scale turns the face planes.
    pub fn scaled(&self, s: Vec3) -> Option<Hull> {
        let pts: Vec<Vec3> = self.verts().iter().map(|v| *v * s).collect();
        Hull::from_points(&pts)
    }
}

/// Whether `com` stands over the area the contact `points` span, seen along
/// `down`: a body resting on a face, rather than balanced on an edge or a
/// corner it will tip off. Two points, or one, span no area, so a body on
/// an edge or a corner is never "over" its support. `margin` keeps a body
/// whose centre is only a hair inside the edge counted as tipping.
pub(crate) fn over_support(points: &[Vec3], com: Vec3, down: Vec3, margin: f32) -> bool {
    let Some(d) = down.try_normalize() else { return true };
    let (u, v) = d.any_orthonormal_pair();
    let flat = |p: Vec3| [p.dot(u), p.dot(v)];
    let mut pts: Vec<[f32; 2]> = points.iter().map(|p| flat(*p)).collect();
    pts.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    pts.dedup_by(|a, b| (a[0] - b[0]).abs() < 1e-4 && (a[1] - b[1]).abs() < 1e-4);
    if pts.len() < 3 {
        return false;
    }
    // Monotone chain: the support outline, counter-clockwise.
    let cross = |o: [f32; 2], a: [f32; 2], b: [f32; 2]| (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0]);
    let mut ring: Vec<[f32; 2]> = Vec::new();
    for pass in 0..2 {
        let start = ring.len();
        let it: Box<dyn Iterator<Item = &[f32; 2]>> = if pass == 0 { Box::new(pts.iter()) } else { Box::new(pts.iter().rev()) };
        for p in it {
            while ring.len() >= start + 2 && cross(ring[ring.len() - 2], ring[ring.len() - 1], *p) <= 1e-9 {
                ring.pop();
            }
            ring.push(*p);
        }
        ring.pop();
    }
    if ring.len() < 3 {
        return false;
    }
    let c = flat(com);
    (0..ring.len()).all(|i| {
        let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
        let len = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt().max(1e-9);
        cross(a, b, c) / len >= margin
    })
}

fn bounds(pts: &[Vec3]) -> (Vec3, Vec3) {
    pts.iter().fold((Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)), |(lo, hi), p| (lo.min(*p), hi.max(*p)))
}

fn dedup(pts: &[Vec3], eps: f32) -> Vec<Vec3> {
    let mut out: Vec<Vec3> = Vec::new();
    for p in pts {
        if !out.iter().any(|q| q.distance_squared(*p) <= eps * eps) {
            out.push(*p);
        }
    }
    out
}

/// The furthest point along each of a few hundred directions spread evenly
/// over the sphere, plus the axes and diagonals.
fn support_points(pts: &[Vec3]) -> Vec<Vec3> {
    let mut dirs: Vec<Vec3> = Vec::new();
    for x in [-1.0f32, 0.0, 1.0] {
        for y in [-1.0f32, 0.0, 1.0] {
            for z in [-1.0f32, 0.0, 1.0] {
                if let Some(d) = Vec3::new(x, y, z).try_normalize() {
                    dirs.push(d);
                }
            }
        }
    }
    let n = 240;
    let golden = std::f32::consts::PI * (3.0 - 5.0f32.sqrt());
    for i in 0..n {
        let y = 1.0 - 2.0 * (i as f32 + 0.5) / n as f32;
        let r = (1.0 - y * y).max(0.0).sqrt();
        let t = golden * i as f32;
        dirs.push(Vec3::new(r * t.cos(), y, r * t.sin()));
    }
    let mut out: Vec<Vec3> = Vec::new();
    for d in dirs {
        let best = pts.iter().copied().fold((f32::MIN, Vec3::ZERO), |b, p| {
            let s = p.dot(d);
            if s > b.0 { (s, p) } else { b }
        });
        if !out.contains(&best.1) {
            out.push(best.1);
        }
    }
    out
}

/// Points that all lie in a plane (or on a line) get copies a centimetre
/// either side of it, so the hull has a volume.
fn thicken(pts: &mut Vec<Vec3>, extent: f32) {
    if pts.len() < 4 || flat_axis(pts, extent).is_some() {
        let axis = flat_axis(pts, extent).unwrap_or(Vec3::Y);
        let t = 0.01f32.min(extent * 0.5).max(1e-4);
        let base = pts.clone();
        pts.clear();
        for p in base {
            pts.push(p + axis * t);
            pts.push(p - axis * t);
        }
        // A line, or a single point: thicken it the other two ways as well.
        if flat_axis(pts, extent).is_some() {
            let base = pts.clone();
            let (u, v) = axis.any_orthonormal_pair();
            for p in base {
                for d in [u, -u, v, -v] {
                    pts.push(p + d * t);
                }
            }
        }
    }
}

/// The normal of the plane every point lies in, when they do.
fn flat_axis(pts: &[Vec3], extent: f32) -> Option<Vec3> {
    let a = pts[0];
    let b = *pts.iter().max_by(|p, q| p.distance_squared(a).total_cmp(&q.distance_squared(a)))?;
    let ab = b - a;
    if ab.length() < extent * 1e-4 {
        return Some(Vec3::Y);
    }
    let c = *pts
        .iter()
        .max_by(|p, q| ab.cross(**p - a).length_squared().total_cmp(&ab.cross(**q - a).length_squared()))?;
    let n = ab.cross(c - a);
    if n.length() < extent * extent * 1e-4 {
        // Collinear: any normal of the line will do.
        return Some(ab.normalize().any_orthonormal_vector());
    }
    let n = n.normalize();
    let off = pts.iter().map(|p| (*p - a).dot(n).abs()).fold(0.0f32, f32::max);
    (off < extent * 1e-4).then_some(n)
}

/// Triangles of the convex hull of `pts`, wound outward, as indices into it.
/// Incremental: a starting tetrahedron, then each point outside the hull so
/// far replaces the faces it can see with a fan to the edge around them.
fn hull_faces(pts: &[Vec3], eps: f32) -> Option<Vec<[usize; 3]>> {
    let n = pts.len();
    if n < 4 {
        return None;
    }
    // The tetrahedron: two far-apart points, the point furthest from their
    // line, the point furthest from that plane.
    let i0 = 0;
    let i1 = (0..n).max_by(|&a, &b| pts[a].distance_squared(pts[i0]).total_cmp(&pts[b].distance_squared(pts[i0])))?;
    let line = pts[i1] - pts[i0];
    let i2 = (0..n).max_by(|&a, &b| {
        line.cross(pts[a] - pts[i0]).length_squared().total_cmp(&line.cross(pts[b] - pts[i0]).length_squared())
    })?;
    let nrm = line.cross(pts[i2] - pts[i0]);
    let i3 = (0..n).max_by(|&a, &b| {
        nrm.dot(pts[a] - pts[i0]).abs().total_cmp(&nrm.dot(pts[b] - pts[i0]).abs())
    })?;
    if nrm.length_squared() < eps * eps || nrm.normalize_or_zero().dot(pts[i3] - pts[i0]).abs() < eps {
        return None;
    }
    let inside = (pts[i0] + pts[i1] + pts[i2] + pts[i3]) * 0.25;
    let mut faces: Vec<[usize; 3]> = Vec::new();
    let orient = |f: [usize; 3]| -> [usize; 3] {
        let (a, b, c) = (pts[f[0]], pts[f[1]], pts[f[2]]);
        if (b - a).cross(c - a).dot(a - inside) < 0.0 { [f[0], f[2], f[1]] } else { f }
    };
    for f in [[i0, i1, i2], [i0, i1, i3], [i0, i2, i3], [i1, i2, i3]] {
        faces.push(orient(f));
    }
    let above = |f: &[usize; 3], p: Vec3| -> f32 {
        let (a, b, c) = (pts[f[0]], pts[f[1]], pts[f[2]]);
        (b - a).cross(c - a).normalize_or_zero().dot(p - a)
    };
    for (pi, &p) in pts.iter().enumerate() {
        if [i0, i1, i2, i3].contains(&pi) {
            continue;
        }
        let seen: Vec<bool> = faces.iter().map(|f| above(f, p) > eps).collect();
        if !seen.iter().any(|&s| s) {
            continue;
        }
        // The horizon: edges of seen faces whose other side is not seen.
        let edges: std::collections::BTreeSet<(usize, usize)> = faces
            .iter()
            .zip(&seen)
            .filter(|(_, s)| **s)
            .flat_map(|(f, _)| (0..3).map(move |k| (f[k], f[(k + 1) % 3])))
            .collect();
        let horizon: Vec<(usize, usize)> =
            edges.iter().copied().filter(|&(a, b)| !edges.contains(&(b, a))).collect();
        let mut kept: Vec<[usize; 3]> =
            faces.iter().zip(&seen).filter(|(_, s)| !**s).map(|(f, _)| *f).collect();
        for (a, b) in horizon {
            kept.push([a, b, pi]);
        }
        faces = kept;
    }
    Some(faces)
}

fn face_verts(faces: &[[usize; 3]]) -> Vec<usize> {
    let mut used: Vec<usize> = faces.iter().flatten().copied().collect();
    used.sort_unstable();
    used.dedup();
    used
}

/// `k` of the points at `idx`, chosen to spread as far apart as they can:
/// the extremes on each axis first, then each next point the one furthest
/// from all chosen so far.
fn spread(pts: &[Vec3], idx: &[usize], k: usize) -> Vec<Vec3> {
    let mut chosen: Vec<usize> = Vec::new();
    for axis in 0..3 {
        let key = |i: &usize| pts[*i].to_array()[axis];
        for pick in [
            idx.iter().copied().min_by(|a, b| key(a).total_cmp(&key(b))),
            idx.iter().copied().max_by(|a, b| key(a).total_cmp(&key(b))),
        ]
        .into_iter()
        .flatten()
        {
            if !chosen.contains(&pick) {
                chosen.push(pick);
            }
        }
    }
    while chosen.len() < k {
        let next = idx.iter().copied().filter(|i| !chosen.contains(i)).max_by(|a, b| {
            let d = |i: usize| chosen.iter().map(|&c| pts[c].distance_squared(pts[i])).fold(f32::MAX, f32::min);
            d(*a).total_cmp(&d(*b))
        });
        match next {
            Some(i) => chosen.push(i),
            None => break,
        }
    }
    chosen.into_iter().map(|i| pts[i]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cube(h: Vec3, at: Vec3) -> Vec<Vec3> {
        let mut v = Vec::new();
        for x in [-1.0f32, 1.0] {
            for y in [-1.0f32, 1.0] {
                for z in [-1.0f32, 1.0] {
                    v.push(at + Vec3::new(x, y, z) * h);
                }
            }
        }
        v
    }

    #[test]
    fn a_box_hulls_to_its_own_corners_faces_and_mass() {
        let h = Hull::from_points(&cube(Vec3::new(1.0, 0.5, 0.25), Vec3::new(2.0, 0.0, 0.0))).unwrap();
        assert_eq!(h.verts().len(), 8);
        assert_eq!(h.planes().count(), 6);
        assert!((h.volume() - 1.0).abs() < 1e-4, "volume {}", h.volume());
        assert!(h.centroid().distance(Vec3::new(2.0, 0.0, 0.0)) < 1e-4);
        // A solid box's moments: m/3 · (b² + c²) for half-sizes b and c.
        let i = h.inertia(3.0);
        assert!((i.x - (0.25 + 0.0625)).abs() < 1e-3, "{i:?}");
        assert!((i.y - (1.0 + 0.0625)).abs() < 1e-3, "{i:?}");
        assert!((i.z - (1.0 + 0.25)).abs() < 1e-3, "{i:?}");
        assert!((h.distance(Vec3::new(2.0, 0.0, 0.0)) + 0.25).abs() < 1e-4);
        assert!((h.distance(Vec3::new(2.0, 2.0, 0.0)) - 1.5).abs() < 1e-4);
        assert_eq!(h.edges().len(), 12, "a box outlines as its twelve edges, not its face diagonals");
    }

    #[test]
    fn points_inside_and_a_dense_mesh_keep_only_the_corners() {
        let mut pts = cube(Vec3::ONE, Vec3::ZERO);
        for i in 0..500 {
            let t = i as f32 / 500.0;
            pts.push(Vec3::new(t * 1.8 - 0.9, (t * 7.0).sin() * 0.9, (t * 3.0).cos() * 0.9));
        }
        let h = Hull::from_points(&pts).unwrap();
        assert_eq!(h.verts().len(), 8);
        assert!((h.volume() - 8.0).abs() < 1e-3);
    }

    #[test]
    fn a_round_mesh_is_thinned_to_the_cap_and_stays_round() {
        let mut pts = Vec::new();
        for i in 0..64 {
            for j in 1..16 {
                let (a, b) = (i as f32 / 64.0 * std::f32::consts::TAU, j as f32 / 16.0 * std::f32::consts::PI);
                pts.push(Vec3::new(b.sin() * a.cos(), b.cos(), b.sin() * a.sin()));
            }
        }
        let h = Hull::from_points(&pts).unwrap();
        assert!(h.verts().len() <= HULL_VERTS);
        let (lo, hi) = h.bounds();
        assert!(lo.max_element() < -0.95 && hi.min_element() > 0.95, "{lo:?} {hi:?}");
        assert!(h.distance(Vec3::ZERO) < -0.5);
    }

    #[test]
    fn a_flat_quad_becomes_a_thin_slab() {
        let pts = [Vec3::new(-1.0, 0.0, -1.0), Vec3::new(1.0, 0.0, -1.0), Vec3::new(1.0, 0.0, 1.0), Vec3::new(-1.0, 0.0, 1.0)];
        let h = Hull::from_points(&pts).unwrap();
        let (lo, hi) = h.bounds();
        assert!((hi.y - lo.y - 0.02).abs() < 1e-4, "{lo:?} {hi:?}");
        assert!((hi.x - 1.0).abs() < 1e-4);
    }
}
