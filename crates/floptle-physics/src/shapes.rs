//! Collision shapes: the [`CollisionShape`] trait (a queryable signed
//! distance + normal) and its implementors — analytic primitives (plane,
//! sphere, box, capsule, extruded polygon), the baked SDF terrain, and the
//! triangle-mesh collider with its spatial hash.

use floptle_core::math::{Quat, Vec2, Vec3};

/// Anything physics can query: a signed distance field with a surface normal.
/// Distance is **positive outside** the solid (in air) and **negative inside**.
/// (A morph-time `t` parameter for fractals is added in a later slice.)
pub trait CollisionShape {
    /// Signed distance from world point `p` to the surface (positive = outside).
    fn distance(&self, p: Vec3) -> f32;
    /// Outward unit surface normal at `p` (direction of increasing distance).
    fn normal(&self, p: Vec3) -> Vec3;
    /// The outward normal ONLY where it's reliable — `None` when the field
    /// cannot resolve a direction here (a point DEEP inside a saturated SDF
    /// interior, where the gradient is zero). `normal()` masks that with a
    /// `Vec3::Y` fallback, which silently misreads a fast, deeply-tunneled
    /// impact's closing speed as its +Y component (≈ 0 for a side/vertical
    /// lithobrake). Contact resolution uses this instead and substitutes a
    /// motion-based normal when it's `None`, so the true crash speed survives.
    /// Analytic shapes are always reliable (the default).
    fn normal_reliable(&self, p: Vec3) -> Option<Vec3> {
        Some(self.normal(p))
    }
    /// Downcast to the sculptable terrain field, if this collider is one — the runtime
    /// terrain API (Lua `terrain.sculpt/dig`) edits the sim's own copy through this so
    /// collision keeps agreeing with the authority field it was cloned from.
    /// A bounding sphere in the shape's own frame, if one is worth having.
    ///
    /// `None` means "no useful bound" — an infinite plane, a terrain field, a
    /// mesh whose extent is not cheap to know. The broadphase treats those as
    /// always-candidates (`floptle/0076`), so a shape that does not answer this
    /// behaves exactly as it did before: the narrow phase still tests it.
    ///
    /// Returning a bound that is too SMALL would silently drop contacts, so the
    /// implementations below are the ones where the bound is exact.
    fn bounds(&self) -> Option<(Vec3, f32)> {
        None
    }
    /// The **surface label** of the nearest face to `p`, when this shape carries
    /// per-face labels at all.
    ///
    /// Labels are opaque strings this crate never interprets — physics has no
    /// business knowing what a material is. The editor happens to fill them with
    /// a map mesh's material-slot names, because that is the string a level
    /// author typed and the one a script wants to branch on.
    ///
    /// `None` for every shape that genuinely has one surface — an analytic box,
    /// a terrain field, an imported model registered without labels. That is the
    /// honest answer and it is why this returns an `Option` rather than a
    /// plausible default: a name that is right for map meshes and quietly wrong
    /// for terrain is worse than no name.
    ///
    /// **Not called by the solver or by any query march.** It costs a
    /// closest-point search of its own, so it is asked only when somebody wants
    /// the answer — see the lazy `hit.material` in `floptle-script`.
    fn face_label(&self, _p: Vec3) -> Option<&str> {
        None
    }
    fn chunk_terrain(&self) -> Option<&ChunkTerrain> {
        None
    }
    fn chunk_terrain_mut(&mut self) -> Option<&mut ChunkTerrain> {
        None
    }
}

/// A signed-distance query result: distance to surface + the outward normal.
#[derive(Debug, Clone, Copy)]
pub struct SdfHit {
    pub distance: f32,
    pub normal: [f32; 3],
}

/// A half-space (infinite floor/wall): solid on the `-normal` side of `point`.
pub struct Plane {
    pub point: Vec3,
    pub normal: Vec3,
}

impl Plane {
    /// A horizontal ground plane at height `y` (solid below, air above).
    pub fn ground(y: f32) -> Self {
        Self { point: Vec3::new(0.0, y, 0.0), normal: Vec3::Y }
    }
}

impl CollisionShape for Plane {
    fn distance(&self, p: Vec3) -> f32 {
        (p - self.point).dot(self.normal.try_normalize().unwrap_or(Vec3::Y))
    }
    fn normal(&self, _p: Vec3) -> Vec3 {
        self.normal.try_normalize().unwrap_or(Vec3::Y)
    }
}

/// A solid analytic sphere — e.g. a planet body to walk on.
pub struct SphereShape {
    pub center: Vec3,
    pub radius: f32,
}

impl CollisionShape for SphereShape {
    fn bounds(&self) -> Option<(Vec3, f32)> {
        Some((self.center, self.radius))
    }
    fn distance(&self, p: Vec3) -> f32 {
        (p - self.center).length() - self.radius
    }
    fn normal(&self, p: Vec3) -> Vec3 {
        (p - self.center).try_normalize().unwrap_or(Vec3::Y)
    }
}

/// A polygon extruded along its local Z — the collider a tile with a hand-drawn
/// outline becomes, and the shape a **slope** actually is.
///
/// ## Why this can exist here at all
///
/// A rigid-body engine built on convex hulls would have to decompose a drawn
/// outline into convex pieces before it could collide with it, and a concave one
/// would either be rejected or silently become its hull — a ramp with a notch
/// filling itself in. This collision core is signed-distance-first, so an
/// extruded polygon is *exact geometry* rather than an approximation of one:
/// the 2D field below is the true distance to the outline for concave shapes as
/// much as convex, and extruding it is the standard slab combination. There is
/// nothing to decompose and nothing to approximate, which is why tile collision
/// could be given a real polygon case and not a bounding box wearing one.
///
/// Points are in the shape's own XY plane, in order, and the winding does not
/// matter — the sign comes from a crossing count, not from the area.
pub struct PolyPrismShape {
    /// The outline, in the prism's local XY. At least three points.
    pts: Vec<Vec2>,
    center: Vec3,
    inv_rot: Quat,
    /// Half the extrusion depth along local Z.
    half_z: f32,
    /// Exact bounding radius about `center`, for the broadphase.
    bound: f32,
}

impl PolyPrismShape {
    /// `pts` are local to `center` (already relative), `rot` orients the prism,
    /// `half_z` is half its depth. Returns `None` for anything that is not a
    /// polygon — two points is a line, and a line collider would be a shape you
    /// could stand on from one side and fall through from the other.
    pub fn new(center: Vec3, pts: &[Vec2], half_z: f32, rot: Quat) -> Option<Self> {
        if pts.len() < 3 {
            return None;
        }
        let half_z = half_z.abs().max(1e-4);
        let bound = pts
            .iter()
            .map(|p| (p.length().powi(2) + half_z * half_z).sqrt())
            .fold(0.0f32, f32::max);
        (bound > 1e-4).then(|| Self {
            pts: pts.to_vec(),
            center,
            inv_rot: rot.inverse(),
            half_z,
            bound,
        })
    }

    /// Signed distance from `p` to the outline in 2D: negative inside.
    ///
    /// Distance is the nearest point on any edge — exact for concave outlines,
    /// where a max-of-half-planes (the convex shortcut) would report a point
    /// outside a notch as being inside it. The sign is an upward crossing count,
    /// which is independent of winding.
    fn plane_sdf(&self, q: Vec2) -> f32 {
        let mut d2 = f32::MAX;
        let mut inside = false;
        let n = self.pts.len();
        for i in 0..n {
            let a = self.pts[i];
            let b = self.pts[(i + 1) % n];
            let e = b - a;
            let w = q - a;
            let t = (w.dot(e) / e.dot(e).max(1e-12)).clamp(0.0, 1.0);
            d2 = d2.min((w - e * t).length_squared());
            // Crossing test: does the horizontal ray from `q` cross this edge?
            if (a.y > q.y) != (b.y > q.y) && q.x < a.x + (q.y - a.y) / (b.y - a.y) * e.x {
                inside = !inside;
            }
        }
        let d = d2.max(0.0).sqrt();
        if inside { -d } else { d }
    }
}

impl CollisionShape for PolyPrismShape {
    fn bounds(&self) -> Option<(Vec3, f32)> {
        Some((self.center, self.bound))
    }

    fn distance(&self, p: Vec3) -> f32 {
        let l = self.inv_rot * (p - self.center);
        let d = self.plane_sdf(Vec2::new(l.x, l.y));
        let dz = l.z.abs() - self.half_z;
        // The standard extrusion: the outside part is the length of the positive
        // components, the inside part is the larger (less negative) of the two.
        let outside = Vec2::new(d.max(0.0), dz.max(0.0)).length();
        outside + d.max(dz).min(0.0)
    }

    fn normal(&self, p: Vec3) -> Vec3 {
        // Finite-difference, exactly as `BoxShape` does — robust on a face, an
        // edge or a corner, and the field is well-conditioned everywhere except
        // the medial axis deep inside.
        let e = 0.005;
        let d = self.distance(p);
        Vec3::new(
            self.distance(p + Vec3::X * e) - d,
            self.distance(p + Vec3::Y * e) - d,
            self.distance(p + Vec3::Z * e) - d,
        )
        .try_normalize()
        .unwrap_or(Vec3::Y)
    }
}

/// A solid oriented box (OBB) — a static collider matching a Cube primitive's geometry.
/// `inv_rot` rotates a world point into the box's local frame; `half` are the local
/// half-extents. Distance is the exact box SDF; the normal is a finite-difference of it
/// (robust for any face/edge/corner, inside or out).
pub struct BoxShape {
    pub center: Vec3,
    pub half: Vec3,
    pub inv_rot: Quat,
}

impl BoxShape {
    /// An oriented box centered at `center`, rotated by `rot`, with local half-extents `half`.
    pub fn new(center: Vec3, half: Vec3, rot: Quat) -> Self {
        Self { center, half: half.abs().max(Vec3::splat(1e-3)), inv_rot: rot.inverse() }
    }
}

impl CollisionShape for BoxShape {
    fn bounds(&self) -> Option<(Vec3, f32)> {
        // The box's own diagonal, so ANY rotation is covered without asking
        // which one this is.
        Some((self.center, self.half.length()))
    }
    fn distance(&self, p: Vec3) -> f32 {
        let l = self.inv_rot * (p - self.center);
        let q = l.abs() - self.half;
        q.max(Vec3::ZERO).length() + q.x.max(q.y.max(q.z)).min(0.0)
    }
    fn normal(&self, p: Vec3) -> Vec3 {
        let e = 0.005;
        let d = self.distance(p);
        Vec3::new(
            self.distance(p + Vec3::X * e) - d,
            self.distance(p + Vec3::Y * e) - d,
            self.distance(p + Vec3::Z * e) - d,
        )
        .try_normalize()
        .unwrap_or(Vec3::Y)
    }
}

/// A solid capsule (a segment `a`→`b` inflated by `radius`) — a static collider matching
/// a Capsule primitive's geometry.
pub struct CapsuleShape {
    pub a: Vec3,
    pub b: Vec3,
    pub radius: f32,
}

impl CapsuleShape {
    fn closest(&self, p: Vec3) -> Vec3 {
        let ab = self.b - self.a;
        let t = ((p - self.a).dot(ab) / ab.dot(ab).max(1e-6)).clamp(0.0, 1.0);
        self.a + ab * t
    }
}

impl CollisionShape for CapsuleShape {
    fn bounds(&self) -> Option<(Vec3, f32)> {
        Some(((self.a + self.b) * 0.5, (self.b - self.a).length() * 0.5 + self.radius))
    }
    fn distance(&self, p: Vec3) -> f32 {
        (p - self.closest(p)).length() - self.radius
    }
    fn normal(&self, p: Vec3) -> Vec3 {
        (p - self.closest(p)).try_normalize().unwrap_or(Vec3::Y)
    }
}

/// An SDF-terrain collider — collides against the **same baked field the renderer
/// draws** (ADR-0012), in the terrain's local space. Owns a snapshot of the field so
/// the physics step is independent of editor state. World placement comes from the
/// [`AnchoredCollider`] anchor (the terrain node's `f64` translation), so a terrain
/// placed millions of units out collides exactly (ADR-0015).
pub struct SdfTerrain {
    pub terrain: floptle_field::Terrain,
}

impl CollisionShape for SdfTerrain {
    fn distance(&self, p: Vec3) -> f32 {
        self.terrain.sample([p.x, p.y, p.z])
    }
    fn normal(&self, p: Vec3) -> Vec3 {
        Vec3::from(self.terrain.normal([p.x, p.y, p.z])).try_normalize().unwrap_or(Vec3::Y)
    }
}

/// The Terrain 2.0 collider: collides against the **same sparse chunk field the mesher
/// extracts the drawn surface from** — the authority the brushes (and, at runtime, Lua)
/// write. Distances saturate at the field's narrow band a few voxels out, which is all
/// a penetration solver ever reads; ray queries step at most a band per iteration.
/// World placement rides the [`AnchoredCollider`] `f64` anchor exactly like
/// [`SdfTerrain`] (ADR-0015), and unlike the dense grid there is **no size cap**: the
/// field is unbounded, so physics finally agrees with the renderer everywhere.
pub struct ChunkTerrain {
    pub field: floptle_field::ChunkField,
    /// The terrain NODE's world rotation — queries rotate into the field's local
    /// frame, so a tilted terrain collides exactly where it draws.
    pub rot: Quat,
    /// The node's UNIFORM scale (x drives; an SDF can't stretch non-uniformly
    /// without breaking the distance metric). Distances scale back up by this.
    pub scale: f32,
}

impl ChunkTerrain {
    pub fn new(field: floptle_field::ChunkField) -> Self {
        Self { field, rot: Quat::IDENTITY, scale: 1.0 }
    }

    /// Anchor-relative world point → the field's local frame.
    #[inline]
    fn to_local(&self, p: Vec3) -> Vec3 {
        (self.rot.inverse() * p) / self.scale.max(1e-6)
    }
}

impl CollisionShape for ChunkTerrain {
    fn distance(&self, p: Vec3) -> f32 {
        self.field.d(self.to_local(p)) * self.scale.max(1e-6)
    }
    fn normal(&self, p: Vec3) -> Vec3 {
        self.normal_reliable(p).unwrap_or(Vec3::Y)
    }
    fn normal_reliable(&self, p: Vec3) -> Option<Vec3> {
        // `try_normalize` yields None where the gradient is zero — i.e. deep in
        // a fully-solid (Uniform(-band)) interior, exactly where a fast ram
        // tunnels to. There the caller falls back to the body's travel axis.
        (self.rot * self.field.grad(self.to_local(p))).try_normalize()
    }
    fn chunk_terrain(&self) -> Option<&ChunkTerrain> {
        Some(self)
    }
    fn chunk_terrain_mut(&mut self) -> Option<&mut ChunkTerrain> {
        Some(self)
    }
}

/// Grid cell index containing `p` (one cell = `cell` units on a side).
fn cell_coord(p: Vec3, cell: f32) -> (i32, i32, i32) {
    ((p.x / cell).floor() as i32, (p.y / cell).floor() as i32, (p.z / cell).floor() as i32)
}

/// Closest point to `p` on triangle `abc` (Ericson, *Real-Time Collision Detection*).
fn closest_point_on_triangle(p: Vec3, a: Vec3, b: Vec3, c: Vec3) -> Vec3 {
    let ab = b - a;
    let ac = c - a;
    let ap = p - a;
    let d1 = ab.dot(ap);
    let d2 = ac.dot(ap);
    if d1 <= 0.0 && d2 <= 0.0 {
        return a;
    }
    let bp = p - b;
    let d3 = ab.dot(bp);
    let d4 = ac.dot(bp);
    if d3 >= 0.0 && d4 <= d3 {
        return b;
    }
    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        let den = d1 - d3;
        return if den.abs() > 1e-12 { a + ab * (d1 / den) } else { a };
    }
    let cp = p - c;
    let d5 = ab.dot(cp);
    let d6 = ac.dot(cp);
    if d6 >= 0.0 && d5 <= d6 {
        return c;
    }
    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        let den = d2 - d6;
        return if den.abs() > 1e-12 { a + ac * (d2 / den) } else { a };
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        let den = (d4 - d3) + (d5 - d6);
        return if den.abs() > 1e-12 { b + (c - b) * ((d4 - d3) / den) } else { b };
    }
    let sum = va + vb + vc;
    if sum.abs() <= 1e-12 {
        return a; // degenerate/zero-area triangle — any vertex is the closest point
    }
    let denom = 1.0 / sum;
    a + ab * (vb * denom) + ac * (vc * denom)
}

/// A static triangle-mesh collider — e.g. an imported map model you walk on. World-space
/// triangles are bucketed into a uniform spatial hash so closest-point queries only test
/// nearby triangles. Distance is UNSIGNED (an imported map is rarely watertight); the body
/// is pushed out along `(p − closest)`, which for a surface you rest on points away from
/// the face. Resolved every substep, so a body never tunnels to the wrong side.
pub struct TriMeshCollider {
    tris: Vec<[Vec3; 3]>,
    /// Per-triangle index into `labels`, parallel to `tris`. **Empty** when this
    /// mesh has no per-face labels, which is the ordinary case — an imported
    /// model is one surface as far as this crate is concerned.
    ///
    /// Kept in lockstep with `tris` by being pushed in the same loop, because
    /// the constructor DROPS degenerate triangles: a label list built from the
    /// caller's index buffer and stored as-is would be off by one from the first
    /// zero-area triangle onward, and would answer the wrong material for
    /// everything after it without failing.
    tri_label: Vec<u16>,
    /// The label strings `tri_label` indexes. Opaque here — see
    /// [`CollisionShape::face_label`].
    labels: Vec<String>,
    cell: f32,
    grid: std::collections::HashMap<(i32, i32, i32), Vec<u32>>,
}

impl TriMeshCollider {
    /// Spatial-hash cell size. The query searches `±SEARCH` cells, so a body up to
    /// `CELL·SEARCH` units in radius is guaranteed to find every triangle within its
    /// reach (5×5×5 block covers radii up to ~4 — far beyond any normal capsule).
    const CELL: f32 = 2.0;
    const SEARCH: i32 = 2;

    pub fn new(verts: &[Vec3], indices: &[u32]) -> Self {
        Self::labelled(verts, indices, &[], Vec::new())
    }

    /// The same collider, carrying one label per SOURCE triangle.
    ///
    /// `tri_label[i]` is the label of the triangle at `indices[i*3..]`, and
    /// indexes `labels`. A short or empty `tri_label` simply leaves those
    /// triangles unlabelled rather than failing — a collider that refused to
    /// exist because a label list was the wrong length would take the level's
    /// collision with it, which is a far worse outcome than an unanswered
    /// `hit.material`.
    pub fn labelled(
        verts: &[Vec3],
        indices: &[u32],
        tri_label: &[u16],
        labels: Vec<String>,
    ) -> Self {
        let cell = Self::CELL;
        let mut tris = Vec::with_capacity(indices.len() / 3);
        let mut kept_label = Vec::new();
        let labelled = !labels.is_empty();
        let mut grid: std::collections::HashMap<(i32, i32, i32), Vec<u32>> =
            std::collections::HashMap::new();
        for (src, tri) in indices.as_chunks::<3>().0.iter().enumerate() {
            let (a, b, c) =
                (verts[tri[0] as usize], verts[tri[1] as usize], verts[tri[2] as usize]);
            // Skip degenerate (zero-area) triangles — common in imported meshes and a
            // source of NaNs in closest-point queries.
            if (b - a).cross(c - a).length_squared() <= 1e-12 {
                continue;
            }
            let ti = tris.len() as u32;
            tris.push([a, b, c]);
            if labelled {
                // Pushed HERE, beside the triangle it belongs to, so dropping a
                // degenerate one cannot shift every label after it.
                kept_label.push(tri_label.get(src).copied().unwrap_or(u16::MAX));
            }
            let lo = cell_coord(a.min(b).min(c), cell);
            let hi = cell_coord(a.max(b).max(c), cell);
            for cz in lo.2..=hi.2 {
                for cy in lo.1..=hi.1 {
                    for cx in lo.0..=hi.0 {
                        grid.entry((cx, cy, cz)).or_default().push(ti);
                    }
                }
            }
        }
        Self { tris, tri_label: kept_label, labels, cell, grid }
    }

    /// Closest point on the mesh to `p` (its squared distance, and which triangle
    /// it is on), searching the ±`SEARCH` cell block around `p`. `None` if no
    /// triangle is within that block.
    fn nearest_tri(&self, p: Vec3) -> Option<(Vec3, f32, u32)> {
        let c = cell_coord(p, self.cell);
        let s = Self::SEARCH;
        let mut best: Option<(Vec3, f32, u32)> = None;
        for cz in (c.2 - s)..=(c.2 + s) {
            for cy in (c.1 - s)..=(c.1 + s) {
                for cx in (c.0 - s)..=(c.0 + s) {
                    let Some(list) = self.grid.get(&(cx, cy, cz)) else { continue };
                    for &ti in list {
                        let t = self.tris[ti as usize];
                        let q = closest_point_on_triangle(p, t[0], t[1], t[2]);
                        let d2 = (p - q).length_squared();
                        // Skip non-finite results defensively (degenerate input).
                        if d2.is_finite() && best.is_none_or(|(_, bd, _)| d2 < bd) {
                            best = Some((q, d2, ti));
                        }
                    }
                }
            }
        }
        best
    }

    /// Closest point on the mesh to `p`, and its squared distance.
    fn nearest(&self, p: Vec3) -> Option<(Vec3, f32)> {
        self.nearest_tri(p).map(|(q, d2, _)| (q, d2))
    }
}

impl CollisionShape for TriMeshCollider {
    fn distance(&self, p: Vec3) -> f32 {
        // No nearby triangle → far away (no collision). Unsigned, so always ≥ 0.
        self.nearest(p).map(|(_, d2)| d2.sqrt()).unwrap_or(1e6)
    }
    fn normal(&self, p: Vec3) -> Vec3 {
        match self.nearest(p) {
            Some((q, _)) => (p - q).try_normalize().unwrap_or(Vec3::Y),
            None => Vec3::Y,
        }
    }
    /// The label of the triangle nearest `p`.
    ///
    /// **The nearest triangle, not the one the march decided on**, because the
    /// march never decided on one: it reports the collider it came within
    /// tolerance of. The closest face at the hit point is the honest answer to
    /// "what did I touch", and it costs a closest-point search — which is why
    /// nothing on the query path calls this and the script layer asks for it
    /// only when a script reads the field.
    fn face_label(&self, p: Vec3) -> Option<&str> {
        let (_, _, ti) = self.nearest_tri(p)?;
        let li = *self.tri_label.get(ti as usize)?;
        self.labels.get(li as usize).map(String::as_str)
    }
}

#[cfg(test)]
mod poly_tests {
    use super::*;

    /// The right triangle a 45° slope tile is: the hypotenuse runs from the
    /// bottom-left to the top-right, so the space ABOVE it is empty.
    fn ramp() -> PolyPrismShape {
        let pts = [Vec2::new(-0.5, -0.5), Vec2::new(0.5, -0.5), Vec2::new(0.5, 0.5)];
        PolyPrismShape::new(Vec3::ZERO, &pts, 0.5, Quat::IDENTITY).expect("a triangle is a polygon")
    }

    /// The whole point of the shape existing. A bounding box would report the
    /// top-left corner as solid, and a character would stand on thin air at the
    /// top of every ramp — which is exactly the failure that kept polygon tile
    /// collision out of the tileset until there was a real shape for it.
    #[test]
    fn a_ramp_is_empty_above_its_slope_and_solid_below() {
        let r = ramp();
        assert!(r.distance(Vec3::new(-0.3, 0.3, 0.0)) > 0.0, "the empty corner reads solid");
        assert!(r.distance(Vec3::new(0.3, -0.3, 0.0)) < 0.0, "the filled corner reads empty");
        // …and the bounding box would have said otherwise about the first one.
        let bx = BoxShape::new(Vec3::ZERO, Vec3::new(0.5, 0.5, 0.5), Quat::IDENTITY);
        assert!(bx.distance(Vec3::new(-0.3, 0.3, 0.0)) < 0.0, "the control is not a control");
    }

    /// The surface a character actually runs up. On the hypotenuse the normal
    /// points up and to the left at 45°, which is what turns horizontal input
    /// into a climb instead of a wall.
    #[test]
    fn the_slope_face_has_the_slopes_normal() {
        let n = ramp().normal(Vec3::new(0.0, 0.02, 0.0));
        assert!(n.z.abs() < 0.05, "a face normal picked up depth: {n:?}");
        let d = std::f32::consts::FRAC_1_SQRT_2;
        assert!((n.x + d).abs() < 0.05 && (n.y - d).abs() < 0.05, "not 45°: {n:?}");
    }

    /// Distance is the true distance to the outline, so a point out in the empty
    /// corner is as far away as the geometry says — a max-of-half-planes
    /// shortcut would under-report it and inflate the shape.
    #[test]
    fn distance_outside_is_the_real_distance_to_the_edge() {
        let d = ramp().distance(Vec3::new(-0.5, 0.5, 0.0));
        // The corner (-0.5, 0.5) is √2/2 from the hypotenuse through the origin.
        let want = std::f32::consts::FRAC_1_SQRT_2;
        assert!((d - want).abs() < 0.02, "expected ~{want}, got {d}");
    }

    /// Concave outlines are allowed and are not quietly filled in. An L needs
    /// the inside of its corner to stay empty; a convex hull would close it.
    #[test]
    fn a_concave_outline_keeps_its_notch() {
        let l = [
            Vec2::new(-0.5, -0.5),
            Vec2::new(0.5, -0.5),
            Vec2::new(0.5, 0.0),
            Vec2::new(0.0, 0.0),
            Vec2::new(0.0, 0.5),
            Vec2::new(-0.5, 0.5),
        ];
        let s = PolyPrismShape::new(Vec3::ZERO, &l, 0.5, Quat::IDENTITY).unwrap();
        assert!(s.distance(Vec3::new(0.25, 0.25, 0.0)) > 0.0, "the notch filled itself in");
        assert!(s.distance(Vec3::new(-0.25, 0.25, 0.0)) < 0.0, "the arm is not solid");
        assert!(s.distance(Vec3::new(0.25, -0.25, 0.0)) < 0.0, "the foot is not solid");
    }

    /// The extrusion is a slab, not an infinite prism: past the depth it is air,
    /// which is what keeps a 2D layer's colliders out of the layer in front.
    #[test]
    fn the_prism_ends_at_its_depth() {
        let r = ramp();
        assert!(r.distance(Vec3::new(0.3, -0.3, 0.0)) < 0.0);
        assert!(r.distance(Vec3::new(0.3, -0.3, 0.9)) > 0.0, "solid beyond the extrusion");
    }

    /// Winding is not the author's problem. The editor writes points in whatever
    /// order they were clicked, and a reversed outline must not be inside-out.
    #[test]
    fn winding_does_not_decide_what_is_inside() {
        let fwd = ramp();
        let pts = [Vec2::new(0.5, 0.5), Vec2::new(0.5, -0.5), Vec2::new(-0.5, -0.5)];
        let rev = PolyPrismShape::new(Vec3::ZERO, &pts, 0.5, Quat::IDENTITY).unwrap();
        for p in [Vec3::new(0.3, -0.3, 0.0), Vec3::new(-0.3, 0.3, 0.0), Vec3::new(0.0, 0.0, 0.2)] {
            assert!(
                (fwd.distance(p) - rev.distance(p)).abs() < 1e-5,
                "reversing the points changed the shape at {p:?}"
            );
        }
    }

    /// Not-a-polygon is not a collider. A line you can stand on from one side
    /// and fall through from the other is worse than nothing there.
    #[test]
    fn fewer_than_three_points_is_not_a_shape() {
        assert!(PolyPrismShape::new(Vec3::ZERO, &[], 0.5, Quat::IDENTITY).is_none());
        assert!(
            PolyPrismShape::new(
                Vec3::ZERO,
                &[Vec2::ZERO, Vec2::new(1.0, 0.0)],
                0.5,
                Quat::IDENTITY
            )
            .is_none()
        );
    }

    /// The broadphase drops anything outside this radius, so a bound that was
    /// too small would silently lose contacts (`floptle/0076`).
    #[test]
    fn the_bound_contains_every_corner() {
        let r = ramp();
        let (c, rad) = r.bounds().expect("a polygon knows its extent");
        for p in [
            Vec3::new(-0.5, -0.5, 0.5),
            Vec3::new(0.5, -0.5, -0.5),
            Vec3::new(0.5, 0.5, 0.5),
        ] {
            assert!((p - c).length() <= rad + 1e-5, "{p:?} is outside the bound {rad}");
        }
    }
}

#[cfg(test)]
mod face_label_tests {
    use super::*;

    /// A floor in two halves: `x < 0` is one material, `x > 0` is another, and a
    /// deliberately degenerate triangle sits between them.
    ///
    /// The degenerate one is the point. `TriMeshCollider` drops zero-area
    /// triangles — they are common in imported meshes and a source of NaNs — so
    /// a label list stored as the caller handed it in goes off by one from the
    /// first dropped triangle onward, and every face after it reports the
    /// material of its neighbour. That fails quietly: the collider works, the
    /// query answers, and the answer is wrong for half the level.
    fn split_floor() -> TriMeshCollider {
        let verts = [
            Vec3::new(-4.0, 0.0, -4.0), // 0
            Vec3::new(0.0, 0.0, -4.0),  // 1
            Vec3::new(0.0, 0.0, 4.0),   // 2
            Vec3::new(-4.0, 0.0, 4.0),  // 3
            Vec3::new(4.0, 0.0, -4.0),  // 4
            Vec3::new(4.0, 0.0, 4.0),   // 5
        ];
        let indices = [
            0, 1, 2, // left half
            0, 2, 3, //
            1, 1, 1, // zero-area: dropped by the constructor
            1, 4, 5, // right half
            1, 5, 2, //
        ];
        let tri_label = [0u16, 0, 0, 1, 1];
        TriMeshCollider::labelled(
            &verts,
            &indices,
            &tri_label,
            vec!["Grass".into(), "Boards".into()],
        )
    }

    #[test]
    fn a_labelled_mesh_says_which_material_the_nearest_face_is() {
        let m = split_floor();
        assert_eq!(m.face_label(Vec3::new(-2.0, 0.4, 0.0)), Some("Grass"));
        // The far side of the degenerate triangle. Before the labels were
        // pushed beside the triangles they belong to, this answered "Grass".
        assert_eq!(m.face_label(Vec3::new(2.0, 0.4, 0.0)), Some("Boards"));
        // Right at the seam it must still answer one of them rather than
        // nothing — a character standing on the join is the ordinary case.
        assert!(m.face_label(Vec3::new(0.0, 0.3, 0.0)).is_some());
    }

    /// **A mesh with no labels answers nothing, not a plausible name.** An
    /// imported model is one surface as far as physics is concerned, and a
    /// material name invented for it would be wrong in a way nothing could
    /// catch — which is worse than the field being absent (`floptle/0174`).
    #[test]
    fn an_unlabelled_mesh_answers_nothing() {
        let verts =
            [Vec3::new(-1.0, 0.0, -1.0), Vec3::new(1.0, 0.0, -1.0), Vec3::new(0.0, 0.0, 1.0)];
        let m = TriMeshCollider::new(&verts, &[0, 1, 2]);
        assert_eq!(m.face_label(Vec3::new(0.0, 0.5, 0.0)), None);
        // …and neither does anything else that has one surface.
        let b = BoxShape::new(Vec3::ZERO, Vec3::splat(1.0), Quat::IDENTITY);
        assert_eq!(b.face_label(Vec3::new(0.0, 2.0, 0.0)), None);
        assert_eq!(Plane::ground(0.0).face_label(Vec3::new(0.0, 1.0, 0.0)), None);
    }

    /// A label index that names no label answers nothing rather than panicking.
    /// Hand-edited sidecars reach this crate, and a collider that takes the
    /// level's collision down with it over a bad material index is a far worse
    /// outcome than an unanswered field.
    #[test]
    fn a_label_index_past_the_end_is_survivable() {
        let verts =
            [Vec3::new(-1.0, 0.0, -1.0), Vec3::new(1.0, 0.0, -1.0), Vec3::new(0.0, 0.0, 1.0)];
        let m = TriMeshCollider::labelled(&verts, &[0, 1, 2], &[9], vec!["Only".into()]);
        assert_eq!(m.face_label(Vec3::new(0.0, 0.5, 0.0)), None);
        assert!(m.distance(Vec3::new(0.0, 0.5, 0.0)) > 0.0, "and it still collides");
        // A label list shorter than the triangles leaves the rest unlabelled,
        // rather than refusing to build the collider at all.
        let short = TriMeshCollider::labelled(&verts, &[0, 1, 2], &[], vec!["Only".into()]);
        assert_eq!(short.face_label(Vec3::new(0.0, 0.5, 0.0)), None);
    }
}
