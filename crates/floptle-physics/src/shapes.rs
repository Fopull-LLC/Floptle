//! Collision shapes: the [`CollisionShape`] trait (a queryable signed
//! distance + normal) and its implementors — analytic primitives (plane,
//! sphere, box, capsule), the baked SDF terrain, and the triangle-mesh
//! collider with its spatial hash.

use floptle_core::math::{Quat, Vec3};

/// Anything physics can query: a signed distance field with a surface normal.
/// Distance is **positive outside** the solid (in air) and **negative inside**.
/// (A morph-time `t` parameter for fractals is added in a later slice.)
pub trait CollisionShape {
    /// Signed distance from world point `p` to the surface (positive = outside).
    fn distance(&self, p: Vec3) -> f32;
    /// Outward unit surface normal at `p` (direction of increasing distance).
    fn normal(&self, p: Vec3) -> Vec3;
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
    fn distance(&self, p: Vec3) -> f32 {
        (p - self.center).length() - self.radius
    }
    fn normal(&self, p: Vec3) -> Vec3 {
        (p - self.center).try_normalize().unwrap_or(Vec3::Y)
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
}

impl CollisionShape for ChunkTerrain {
    fn distance(&self, p: Vec3) -> f32 {
        self.field.d(p)
    }
    fn normal(&self, p: Vec3) -> Vec3 {
        self.field.grad(p).try_normalize().unwrap_or(Vec3::Y)
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
        let cell = Self::CELL;
        let mut tris = Vec::with_capacity(indices.len() / 3);
        let mut grid: std::collections::HashMap<(i32, i32, i32), Vec<u32>> =
            std::collections::HashMap::new();
        for tri in indices.chunks_exact(3) {
            let (a, b, c) =
                (verts[tri[0] as usize], verts[tri[1] as usize], verts[tri[2] as usize]);
            // Skip degenerate (zero-area) triangles — common in imported meshes and a
            // source of NaNs in closest-point queries.
            if (b - a).cross(c - a).length_squared() <= 1e-12 {
                continue;
            }
            let ti = tris.len() as u32;
            tris.push([a, b, c]);
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
        Self { tris, cell, grid }
    }

    /// Closest point on the mesh to `p` (and its squared distance), searching the
    /// ±`SEARCH` cell block around `p`. `None` if no triangle is within that block.
    fn nearest(&self, p: Vec3) -> Option<(Vec3, f32)> {
        let c = cell_coord(p, self.cell);
        let s = Self::SEARCH;
        let mut best: Option<(Vec3, f32)> = None;
        for cz in (c.2 - s)..=(c.2 + s) {
            for cy in (c.1 - s)..=(c.1 + s) {
                for cx in (c.0 - s)..=(c.0 + s) {
                    let Some(list) = self.grid.get(&(cx, cy, cz)) else { continue };
                    for &ti in list {
                        let t = self.tris[ti as usize];
                        let q = closest_point_on_triangle(p, t[0], t[1], t[2]);
                        let d2 = (p - q).length_squared();
                        // Skip non-finite results defensively (degenerate input).
                        if d2.is_finite() && best.is_none_or(|(_, bd)| d2 < bd) {
                            best = Some((q, d2));
                        }
                    }
                }
            }
        }
        best
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
}
