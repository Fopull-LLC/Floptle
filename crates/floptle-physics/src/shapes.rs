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
    /// The outward normal only where it's reliable — `None` when the field
    /// cannot resolve a direction here (a point deep inside a saturated SDF
    /// interior, where the gradient is zero). `normal()` masks that with a
    /// `Vec3::Y` fallback, which silently misreads a fast, deeply-tunneled
    /// impact's closing speed as its +Y component (≈ 0 for a side/vertical
    /// lithobrake). Contact resolution uses this instead and substitutes a
    /// motion-based normal when it's `None`, so the true crash speed survives.
    /// Analytic shapes are always reliable (the default).
    fn normal_reliable(&self, p: Vec3) -> Option<Vec3> {
        Some(self.normal(p))
    }
    /// A bounding sphere in the shape's own frame, if one is worth having.
    ///
    /// `None` means no useful bound: an infinite plane, a terrain field, a
    /// mesh whose extent is not cheap to know. The broadphase keeps those as
    /// candidates always, so the narrow phase still tests them.
    ///
    /// A bound that is too small drops contacts, so only the shapes whose
    /// bound is exact answer.
    fn bounds(&self) -> Option<(Vec3, f32)> {
        None
    }
    /// The surface label of the nearest face to `p`, when this shape carries
    /// per-face labels at all.
    ///
    /// Labels are opaque strings this crate never interprets. The editor fills
    /// them with a map mesh's material-slot names, the string a level author
    /// typed and a script branches on.
    ///
    /// `None` for a shape with one surface: an analytic box, a terrain field,
    /// an imported model registered without labels. An `Option` rather than a
    /// plausible default, because a name that is right for map meshes and
    /// wrong for terrain is worse than no name.
    ///
    /// Costs a closest-point search of its own, so neither the solver nor any
    /// query march calls it; `hit.material` in `floptle-script` asks lazily.
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

/// A polygon extruded along its local Z: the collider a tile with a hand-drawn
/// outline becomes, and the shape a slope is.
///
/// The collision core is signed-distance-first, so an extruded polygon is exact
/// geometry: the 2D field below is the true distance to the outline, concave
/// as much as convex, and extruding it is the standard slab combination.
/// Nothing is decomposed into convex pieces and a notch in a ramp stays a
/// notch.
///
/// Points are in the shape's own XY plane, in order; the winding does not
/// matter, since the sign comes from a crossing count rather than the area.
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
        // The box's own diagonal, so any rotation is covered without asking
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
/// draws**, in the terrain's local space. Owns a snapshot of the field so
/// the physics step is independent of editor state. World placement comes from the
/// [`crate::AnchoredCollider`] anchor (the terrain node's `f64` translation), so a terrain
/// placed millions of units out collides exactly.
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

/// The cached surface of a [`ChunkTerrain`]: the **same triangles the renderer
/// draws**, bucketed for closest-point queries.
///
/// Chunks are meshed on demand — only where a body has actually asked about the
/// terrain — and the answer is kept until the field is written. A whole planet
/// is never meshed for physics; a level's worth of standing and walking touches
/// a handful of chunks.
#[derive(Default)]
struct Surface {
    /// Which chunks have been meshed, so an empty one is not re-meshed every
    /// query. A chunk of pure air or pure rock extracts nothing and belongs in
    /// here exactly as much as one that extracted a thousand triangles.
    meshed: std::collections::HashSet<[i32; 3]>,
    /// Triangles in field-local space, bucketed by `cell`. One grid across every
    /// meshed chunk rather than one per chunk: a query near a chunk boundary
    /// then reads one bucket set instead of merging several.
    grid: std::collections::HashMap<(i32, i32, i32), Vec<SurfaceTri>>,
}

/// One drawn triangle: its corners and the field-gradient normal at each.
///
/// The normals are what decide inside from outside. They are the same normals
/// the renderer shades with — sampled from the field's gradient at each vertex
/// (`mesher.rs`, "the one choice that matters") — interpolated across the
/// face, so the side test is smooth across a curved surface and agrees with
/// the lit picture rather than with a flat facet.
#[derive(Clone, Copy)]
struct SurfaceTri {
    p: [Vec3; 3],
    n: [Vec3; 3],
}

/// The terrain collider: collides against the triangles the mesher extracts,
/// which is what you see.
///
/// The field and the drawn mesh are two different surfaces. Surface nets puts
/// one vertex per surface cell at the mean of that cell's edge crossings and
/// joins them with flat triangles, so the drawn surface chords across every
/// curve in the field, inside a bulge and outside a hollow, by up to about half
/// a voxel. A collider on the field lets a player sink into a hillside drawn
/// solid and hover over ground drawn beneath their feet.
///
/// So both the magnitude and the sign come from the triangles: the magnitude
/// is the distance to the closest drawn point, and the side is decided by the
/// mesh's interpolated vertex normal there, the same field-gradient normal the
/// picture is lit with. Surface nets extracts a closed surface, so that test is
/// well posed everywhere a body can be. Taking the sign from the field instead
/// keeps the field's zero crossing, and a body settles on the field again: at
/// a 1.5 voxel scaled ×5 that is over a metre from the drawn ground.
///
/// Outside the field's narrow band the field saturates and no triangle is
/// within reach, so those queries fall through to the field at the cost of one
/// hash lookup.
///
/// World placement rides the [`crate::AnchoredCollider`] `f64` anchor like
/// [`SdfTerrain`], and there is no size cap: the field is unbounded.
pub struct ChunkTerrain {
    pub field: floptle_field::ChunkField,
    /// The terrain node's world rotation — queries rotate into the field's local
    /// frame, so a tilted terrain collides exactly where it draws.
    pub rot: Quat,
    /// The node's uniform scale (x drives; an SDF can't stretch non-uniformly
    /// without breaking the distance metric). Distances scale back up by this.
    pub scale: f32,
    /// Collide against the extracted triangles rather than the field itself.
    ///
    /// On by default, because agreeing with the picture is the whole point. Off
    /// is the pre-0.92 behaviour, kept reachable for two reasons: a headless
    /// server that never meshes anything can skip the work, and a bug in here
    /// should be answerable with "turn it off" rather than with a release.
    pub mesh_accurate: bool,
    surface: std::cell::RefCell<Surface>,
}

impl ChunkTerrain {
    pub fn new(field: floptle_field::ChunkField) -> Self {
        Self {
            field,
            rot: Quat::IDENTITY,
            scale: 1.0,
            mesh_accurate: true,
            surface: Default::default(),
        }
    }

    /// The same collider, in the pose a terrain node gives it.
    pub fn posed(field: floptle_field::ChunkField, rot: Quat, scale: f32) -> Self {
        Self { rot, scale, ..Self::new(field) }
    }

    /// Anchor-relative world point → the field's local frame.
    #[inline]
    fn to_local(&self, p: Vec3) -> Vec3 {
        (self.rot.inverse() * p) / self.scale.max(1e-6)
    }

    /// **How far a query looks** for a triangle — the field's own narrow band.
    ///
    /// The band is exactly how far the field carries a meaningful distance, so
    /// a query that finds no triangle inside it is a query the field could not
    /// have answered precisely either: past this the two agree, and the reject
    /// in `closest_surface` hands those straight back to the field.
    ///
    /// Separate from [`Self::bucket`], which is how the triangles are filed.
    /// They were one constant to begin with and that cost 11x — see the note
    /// there.
    #[inline]
    fn reach(&self) -> f32 {
        self.field.band().max(self.field.voxel()).max(1e-3)
    }

    /// Bucket size for the triangle grid — **one voxel**, which is about one
    /// triangle.
    ///
    /// Deliberately not the reach. Sizing the buckets to how far a query may
    /// look puts every triangle within the whole band into the bucket the point
    /// is standing in, so the first and commonest lookup — a body resting on
    /// flat ground — walks hundreds of triangles to find the one under its
    /// feet. Buckets this size hold a handful each, and the expanding-ring
    /// search below stops as soon as the answer cannot be beaten, so the reach
    /// costs nothing until something actually makes it look that far.
    #[inline]
    fn bucket(&self) -> f32 {
        self.field.voxel().max(1e-3)
    }

    /// **Forget every meshed chunk.** Called wherever the field is handed out
    /// for writing — a sculpt, a dig, a streamed chunk arriving — because a
    /// cached triangle is a claim about voxels that have just changed.
    ///
    /// Whole-cache rather than per-chunk: the caller gets `&mut ChunkField` and
    /// may write anywhere in it, so there is no edited region to narrow to. The
    /// cost is re-meshing the handful of chunks a body is standing in, on the
    /// next query after an edit, which is precisely when the collision has to be
    /// right.
    pub fn invalidate_surface(&mut self) {
        self.surface.get_mut().meshed.clear();
        self.surface.get_mut().grid.clear();
    }

    /// Make sure every chunk within `cell()` of `local` has been meshed.
    fn ensure_meshed(&self, local: Vec3, s: &mut Surface) {
        let r = Vec3::splat(self.reach());
        for c in self.field.chunks_in_world_box(local - r, local + r) {
            if !s.meshed.insert(c) {
                continue;
            }
            let m = drawn_chunk(&self.field, c);
            let origin = Vec3::from(m.origin);
            let cell = self.bucket();
            for t in m.indices.as_chunks::<3>().0 {
                let at = |i: u32| origin + Vec3::from(m.positions[i as usize]);
                let tri = [at(t[0]), at(t[1]), at(t[2])];
                if (tri[1] - tri[0]).cross(tri[2] - tri[0]).length_squared() <= 1e-12 {
                    continue; // zero-area: no closest point worth having
                }
                // The mesher's normals are unit gradients; a degenerate one
                // (deep in uniform solid, which never meshes) falls back to
                // the face so the side test always has something to compare
                // against rather than a zero.
                let face = (tri[1] - tri[0]).cross(tri[2] - tri[0]).normalize();
                let nrm = |i: u32| {
                    Vec3::from(m.normals[i as usize]).try_normalize().unwrap_or(face)
                };
                let entry = SurfaceTri { p: tri, n: [nrm(t[0]), nrm(t[1]), nrm(t[2])] };
                let lo = cell_coord(tri[0].min(tri[1]).min(tri[2]), cell);
                let hi = cell_coord(tri[0].max(tri[1]).max(tri[2]), cell);
                for cz in lo.2..=hi.2 {
                    for cy in lo.1..=hi.1 {
                        for cx in lo.0..=hi.0 {
                            s.grid.entry((cx, cy, cz)).or_default().push(entry);
                        }
                    }
                }
            }
        }
    }

    /// The closest point on the drawn surface to `local`, and the surface's
    /// outward normal there, if one is in reach.
    fn closest_surface(&self, local: Vec3) -> Option<(Vec3, Vec3)> {
        if !self.mesh_accurate {
            return None;
        }
        // Cheap reject before any meshing: outside the band the field saturates
        // and there is nothing for a triangle to correct.
        if self.field.d(local).abs() > self.reach() {
            return None;
        }
        let mut s = self.surface.borrow_mut();
        self.ensure_meshed(local, &mut s);
        let cell = self.bucket();
        let c = cell_coord(local, cell);
        let mut best = f32::INFINITY;
        let mut hit: Option<(Vec3, Vec3)> = None;
        // Expanding rings, stopping as soon as the answer is provably final. A
        // body resting on the ground asks this several times per substep and
        // the hit is nearly always a triangle directly beneath it, in the first
        // bucket, so a fixed 3×3×3 walk would do twenty-seven buckets of work
        // for one.
        //
        // The stopping rule is exact: after every shell out to `r`, everything
        // still unscanned lies outside the box `[c-r, c+r+1] * cell`, so the
        // shortest distance from the point to that box's faces is a floor on
        // any unscanned triangle. A hit nearer than that floor cannot be
        // beaten.
        //
        // The ring is bounded by the reach the reject above established; past
        // it there is no triangle worth finding and the field answers.
        let max_ring = (self.reach() / cell).ceil().max(1.0) as i32;
        for r in 0..=max_ring {
            for cz in c.2 - r..=c.2 + r {
                for cy in c.1 - r..=c.1 + r {
                    for cx in c.0 - r..=c.0 + r {
                        // Only the shell: the inside was scanned at a smaller r.
                        let on_shell = (cx - c.0).abs() == r
                            || (cy - c.1).abs() == r
                            || (cz - c.2).abs() == r;
                        if !on_shell {
                            continue;
                        }
                        let Some(bucket) = s.grid.get(&(cx, cy, cz)) else { continue };
                        for tri in bucket {
                            let q = closest_point_on_triangle(local, tri.p[0], tri.p[1], tri.p[2]);
                            let d2 = (q - local).length_squared();
                            if d2 < best {
                                best = d2;
                                hit = Some((q, normal_at(tri, q)));
                            }
                        }
                    }
                }
            }
            if hit.is_some() {
                // The scanned box, and how far its nearest face is.
                let lo = Vec3::new((c.0 - r) as f32, (c.1 - r) as f32, (c.2 - r) as f32) * cell;
                let hi = lo + Vec3::splat(cell * (2 * r + 1) as f32);
                let floor = (local - lo).min(hi - local).min_element();
                if floor > 0.0 && best <= floor * floor {
                    break;
                }
            }
        }
        hit
    }
}

/// The triangles of one chunk as drawn: stride 1, no skirt — the LOD-0
/// extraction, which is what the renderer draws up close (`skirt: lod > 0` in
/// the remesh queue). A skirt is a curtain hung over the crack between two
/// LODs; as collision it would be an invisible wall around every chunk.
///
/// One function, because two callers must not disagree about it: the
/// collider meshes through this, and so does [`drawn_surface`], which is what
/// the editor draws when asked to show the collision surface.
pub fn drawn_chunk(field: &floptle_field::ChunkField, c: [i32; 3]) -> floptle_field::ChunkMesh {
    floptle_field::mesh_chunk(field, c, 1, false)
}

/// Every triangle a [`ChunkTerrain`] over `field` collides with, in the
/// field's local frame — for drawing the collision surface over the picture,
/// so "does the collider match what I see" is a question answered by looking.
pub fn drawn_surface(field: &floptle_field::ChunkField) -> Vec<[Vec3; 3]> {
    let mut out = Vec::new();
    for c in field.chunk_coords() {
        let m = drawn_chunk(field, c);
        let origin = Vec3::from(m.origin);
        for t in m.indices.as_chunks::<3>().0 {
            let at = |i: u32| origin + Vec3::from(m.positions[i as usize]);
            out.push([at(t[0]), at(t[1]), at(t[2])]);
        }
    }
    out
}

/// The surface normal at `q`, a point on triangle `tri`: the vertex normals
/// blended by `q`'s barycentric weights.
fn normal_at(tri: &SurfaceTri, q: Vec3) -> Vec3 {
    let [a, b, c] = tri.p;
    let (v0, v1, v2) = (b - a, c - a, q - a);
    let (d00, d01, d11, d20, d21) =
        (v0.dot(v0), v0.dot(v1), v1.dot(v1), v2.dot(v0), v2.dot(v1));
    let den = d00 * d11 - d01 * d01;
    let (wb, wc) = if den.abs() > 1e-12 {
        ((d11 * d20 - d01 * d21) / den, (d00 * d21 - d01 * d20) / den)
    } else {
        (0.0, 0.0)
    };
    let wa = 1.0 - wb - wc;
    let n = tri.n[0] * wa + tri.n[1] * wb + tri.n[2] * wc;
    n.try_normalize().unwrap_or_else(|| v0.cross(v1).normalize_or_zero())
}

impl CollisionShape for ChunkTerrain {
    fn distance(&self, p: Vec3) -> f32 {
        let local = self.to_local(p);
        let scale = self.scale.max(1e-6);
        match self.closest_surface(local) {
            // Magnitude and side from the drawn surface: which way of the
            // closest point the point lies, judged by the surface's own
            // normal there. Exactly on the surface both are zero, and the
            // sign of a zero does not matter.
            Some((q, n)) => {
                let off = local - q;
                let side = if off.dot(n) < 0.0 { -1.0 } else { 1.0 };
                off.length() * side * scale
            }
            None => self.field.d(local) * scale,
        }
    }
    fn normal(&self, p: Vec3) -> Vec3 {
        self.normal_reliable(p).unwrap_or(Vec3::Y)
    }
    fn normal_reliable(&self, p: Vec3) -> Option<Vec3> {
        let local = self.to_local(p);
        // Away from the closest point on the drawn surface — flipped when the
        // point is inside, so a body pushed out is pushed out — so the
        // direction a body is pushed matches the face it is resting on. A
        // point exactly on the surface has no direction of its own and takes
        // the surface's.
        if let Some((q, n)) = self.closest_surface(local) {
            let off = local - q;
            let side = if off.dot(n) < 0.0 { -1.0 } else { 1.0 };
            let dir = (off * side).try_normalize().unwrap_or(n);
            return (self.rot * dir).try_normalize();
        }
        // `try_normalize` yields None where the gradient is zero — i.e. deep in
        // a fully-solid (Uniform(-band)) interior, exactly where a fast ram
        // tunnels to. There the caller falls back to the body's travel axis.
        (self.rot * self.field.grad(local)).try_normalize()
    }
    fn chunk_terrain(&self) -> Option<&ChunkTerrain> {
        Some(self)
    }
    fn chunk_terrain_mut(&mut self) -> Option<&mut ChunkTerrain> {
        Some(self)
    }
}

/// Grid cell index containing `p` (one cell = `cell` units on a side).
/// Squared distance from `p` to the axis-aligned box of one grid cell. Zero when
/// `p` is inside it.
fn cell_min_dist2(p: Vec3, c: (i32, i32, i32), cell: f32) -> f32 {
    let lo = Vec3::new(c.0 as f32, c.1 as f32, c.2 as f32) * cell;
    let d = (lo - p).max(p - (lo + Vec3::splat(cell))).max(Vec3::ZERO);
    d.length_squared()
}

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
/// nearby triangles. Distance is unsigned (an imported map is rarely watertight); the body
/// is pushed out along `(p − closest)`, which for a surface you rest on points away from
/// the face. Resolved every substep, so a body never tunnels to the wrong side.
pub struct TriMeshCollider {
    tris: Vec<[Vec3; 3]>,
    /// A sphere around every triangle, for the world's broadphase.
    ///
    /// Without it a mesh was filed with an infinite radius and offered to
    /// every query — and answering one costs a 5×5×5 walk of this mesh's own
    /// cell grid whether the probe is touching the mesh or on the other side
    /// of the level. A level of a couple of hundred meshes paid that walk a
    /// couple of hundred times per body per tick, almost all of it for meshes
    /// nowhere near the body. Computed once here; a query outside the sphere
    /// cannot be in contact with anything inside it, so the narrowing is pure.
    bound: (Vec3, f32),
    /// The inclusive cell range the grid actually holds, so a query block can be
    /// clamped to it instead of asking the hash about empty space. `(1, 0)`-style
    /// inverted ranges are exactly what an empty mesh wants: the loops run zero
    /// times.
    cells: ((i32, i32, i32), (i32, i32, i32)),
    /// Per-triangle index into `labels`, parallel to `tris`. **Empty** when this
    /// mesh has no per-face labels, which is the ordinary case — an imported
    /// model is one surface as far as this crate is concerned.
    ///
    /// Kept in lockstep with `tris` by being pushed in the same loop, because
    /// the constructor drops degenerate triangles: a label list built from the
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

    /// The same collider, carrying one label per source triangle.
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
                // Pushed here, beside the triangle it belongs to, so dropping a
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
        let bound = if tris.is_empty() {
            (Vec3::ZERO, 0.0)
        } else {
            let (mut lo, mut hi) = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY));
            for t in &tris {
                for v in t {
                    lo = lo.min(*v);
                    hi = hi.max(*v);
                }
            }
            let centre = (lo + hi) * 0.5;
            // Radius from the centre to the farthest vertex, not the box's
            // half-diagonal: tighter, and exact for the sphere test the grid does.
            let r = tris.iter().flatten().map(|v| (*v - centre).length()).fold(0.0, f32::max);
            (centre, r)
        };
        let cells = grid.keys().fold(
            ((i32::MAX, i32::MAX, i32::MAX), (i32::MIN, i32::MIN, i32::MIN)),
            |(lo, hi), k| {
                ((lo.0.min(k.0), lo.1.min(k.1), lo.2.min(k.2)),
                 (hi.0.max(k.0), hi.1.max(k.1), hi.2.max(k.2)))
            },
        );
        Self { tris, tri_label: kept_label, labels, cell, grid, bound, cells }
    }

    /// Closest point on the mesh to `p` (its squared distance, and which triangle
    /// it is on), searching the ±`SEARCH` cell block around `p`. `None` if no
    /// triangle is within that block.
    ///
    /// The block is 125 cells, and a body standing on a mesh floor asks
    /// `distance()` at every sample centre, on every depenetration pass, on
    /// every tick, so this is the cost of a mesh. Two exact narrowings, neither
    /// of which changes the answer:
    ///
    /// * Cells outside this mesh's own extent hold nothing, so they are clamped
    ///   away before the hash is asked.
    /// * A cell every point of which is strictly farther than the best found so
    ///   far can neither beat it nor tie it, so skipping it leaves `best`
    ///   bit-identical to the full scan, triangle index included, which a
    ///   rollback re-simulation needs.
    ///
    /// A triangle is registered in every cell its bounding box touches, so the
    /// cell containing its closest point always holds it too: a triangle within
    /// reach is still found even when the cell it was skipped in was farther.
    fn nearest_tri(&self, p: Vec3) -> Option<(Vec3, f32, u32)> {
        let c = cell_coord(p, self.cell);
        let s = Self::SEARCH;
        let mut best: Option<(Vec3, f32, u32)> = None;
        // A bound to skip against, established before the ordered walk starts —
        // otherwise the walk begins at the far corner of the block and has to
        // work its way inward before it knows anything, which is the worst case
        // for skipping and the common case for a body standing on a surface.
        // The query point's own cell almost always holds the answer, so ask it
        // first and keep the distance only; the triangle is still picked by the
        // ordered walk below, so an exact tie resolves exactly as it always did.
        let mut bound = self.cell_best_d2(p, c, f32::INFINITY);
        let (lo, hi) = self.cells;
        for cz in (c.2 - s).max(lo.2)..=(c.2 + s).min(hi.2) {
            for cy in (c.1 - s).max(lo.1)..=(c.1 + s).min(hi.1) {
                for cx in (c.0 - s).max(lo.0)..=(c.0 + s).min(hi.0) {
                    if cell_min_dist2(p, (cx, cy, cz), self.cell) > bound {
                        continue;
                    }
                    let Some(list) = self.grid.get(&(cx, cy, cz)) else { continue };
                    for &ti in list {
                        let t = self.tris[ti as usize];
                        let q = closest_point_on_triangle(p, t[0], t[1], t[2]);
                        let d2 = (p - q).length_squared();
                        // Skip non-finite results defensively (degenerate input).
                        if d2.is_finite() && best.is_none_or(|(_, bd, _)| d2 < bd) {
                            best = Some((q, d2, ti));
                            bound = bound.min(d2);
                        }
                    }
                }
            }
        }
        best
    }

    /// The smallest squared distance from `p` to any triangle registered in one
    /// cell, or `seed` if the cell holds none. Distance only — see `nearest_tri`.
    fn cell_best_d2(&self, p: Vec3, c: (i32, i32, i32), seed: f32) -> f32 {
        let Some(list) = self.grid.get(&c) else { return seed };
        list.iter().fold(seed, |acc, &ti| {
            let t = self.tris[ti as usize];
            let d2 = (p - closest_point_on_triangle(p, t[0], t[1], t[2])).length_squared();
            if d2.is_finite() && d2 < acc { d2 } else { acc }
        })
    }

    /// Closest point on the mesh to `p`, and its squared distance.
    fn nearest(&self, p: Vec3) -> Option<(Vec3, f32)> {
        self.nearest_tri(p).map(|(q, d2, _)| (q, d2))
    }
}

impl CollisionShape for TriMeshCollider {
    fn bounds(&self) -> Option<(Vec3, f32)> {
        Some(self.bound)
    }
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
    /// bottom-left to the top-right, so the space above it is empty.
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
    /// too small would silently lose contacts.
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
    /// degenerate triangle sits between them.
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
    /// catch — which is worse than the field being absent.
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

#[cfg(test)]
mod mesh_bound_tests {
    use super::*;

    /// A mesh has a bound, and it is the right one. `None` would file it as
    /// "everywhere" and every body would test every mesh in the level every
    /// tick. The bound must contain every vertex (a vertex outside it is a
    /// contact the broadphase would drop) and must not be the infinite one.
    #[test]
    fn a_mesh_reports_a_sphere_that_contains_every_vertex() {
        let verts = [
            Vec3::new(10.0, 0.0, 10.0),
            Vec3::new(14.0, 0.0, 10.0),
            Vec3::new(10.0, 3.0, 10.0),
            Vec3::new(12.0, 1.0, 16.0),
        ];
        let idx = [0u32, 1, 2, 1, 2, 3];
        let m = TriMeshCollider::new(&verts, &idx);
        let (c, r) = m.bounds().expect("a mesh must narrow, not be offered to every query");
        assert!(r.is_finite() && r > 0.0, "radius {r}");
        for v in &verts {
            assert!((*v - c).length() <= r + 1e-4, "vertex {v} outside the bound ({c}, {r})");
        }
        // And it is tight enough to be worth having: a probe well away from
        // the mesh must fall outside it.
        assert!((Vec3::new(-50.0, 0.0, -50.0) - c).length() > r);
    }
}

#[cfg(test)]
mod drawn_surface_tests {
    use super::*;
    use floptle_field::{Brush, BrushProfile, ChunkField};

    /// A rounded hill: curvature is what makes surface nets and the field
    /// disagree, so a flat plane would prove nothing.
    fn hill() -> ChunkField {
        let mut f = ChunkField::new(0.5);
        for _ in 0..30 {
            f.sculpt(Brush::Raise, Vec3::new(0.0, 0.0, 0.0), 6.0, 1.0, BrushProfile::default());
        }
        f
    }

    /// Rough ground — many small overlapping stamps, which is what a generated
    /// or hand-sculpted landscape actually is. Smooth curvature is the easy case
    /// for surface nets; detail near the voxel size is where the extracted
    /// triangles and the field part company.
    fn rough() -> ChunkField {
        let mut f = ChunkField::new(0.5);
        for _ in 0..30 {
            f.sculpt(Brush::Raise, Vec3::new(0.0, 0.0, 0.0), 6.0, 1.0, BrushProfile::default());
        }
        let mut seed = 12345u32;
        let mut rnd = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as f32 / 16777216.0
        };
        for _ in 0..40 {
            let c = Vec3::new(rnd() * 10.0 - 5.0, rnd() * 4.0 - 1.0, rnd() * 10.0 - 5.0);
            let b = if rnd() > 0.5 { Brush::Raise } else { Brush::Lower };
            f.sculpt(b, c, 1.0 + rnd() * 1.5, 1.0, BrushProfile::default());
        }
        f
    }

    /// What agreeing with the picture costs a step, as a ratio against the
    /// field collider, never as a duration, so runner speed cancels.
    ///
    /// Twenty bodies resting on rough ground, the state a game is in almost
    /// all of the time and the one the query is asked hardest. About 3.2×: 39
    /// µs against 123 µs a step here, under one percent of a frame.
    ///
    /// The ceiling sits close to that on purpose. Triangle buckets sized to the
    /// query reach rather than to a voxel put every triangle in the band into
    /// the bucket a resting body stands in, and finding the triangle under its
    /// feet walked hundreds of them; a comfortable ceiling would let that
    /// through.
    #[test]
    fn colliding_with_the_drawn_surface_is_not_dear() {
        let cost = |accurate: bool| {
            let mut t = ChunkTerrain::new(rough());
            t.mesh_accurate = accurate;
            let mut w = crate::PhysicsWorld::new(Default::default());
            w.add_collider(Box::new(t));
            for i in 0..20 {
                let a = i as f32 * 0.31;
                w.add_body(crate::Body::sphere(
                    Vec3::new(a.cos() * 4.0, 6.0, a.sin() * 4.0),
                    0.4,
                ));
            }
            for _ in 0..120 {
                w.step(1.0 / 60.0); // let them land — settle is not the subject
            }
            let t0 = std::time::Instant::now();
            for _ in 0..60 {
                w.step(1.0 / 60.0);
            }
            t0.elapsed().as_secs_f64() / 60.0
        };
        // The field first, so a cold cache and a cold branch predictor are not
        // charged to the feature.
        let field = cost(false);
        let mesh = cost(true);
        let ratio = mesh / field.max(1e-9);
        eprintln!("step cost: field {:.0} us, mesh {:.0} us, ratio {ratio:.2}x", field * 1e6, mesh * 1e6);
        assert!(
            ratio < 6.0,
            "colliding against the drawn triangles costs {ratio:.1}x the field collider \
             ({:.0} µs against {:.0} µs a step). Measured at 3.2x; past 6 something has \
             stopped narrowing — check that the triangle buckets are still one VOXEL and \
             not one reach, and that the expanding-ring search still stops early.",
            mesh * 1e6,
            field * 1e6
        );
    }

    /// The cached surface must be a cache — meshed once per chunk, not per
    /// query. A body resting on terrain asks `distance` several times a
    /// substep, and surface-nets extraction is 1-2 ms a chunk; doing it per
    /// query would be a frame budget gone on standing still.
    #[test]
    #[ignore = "timing, not a guard"]
    fn the_surface_cache_is_asked_once_per_chunk() {
        let t = ChunkTerrain::new(rough());
        let probe = Vec3::new(0.0, 3.0, 0.0);
        let t0 = std::time::Instant::now();
        let first = t.distance(probe);
        let cold = t0.elapsed();
        let t1 = std::time::Instant::now();
        for _ in 0..1000 {
            std::hint::black_box(t.distance(probe));
        }
        let warm = t1.elapsed() / 1000;
        println!("cold {cold:?}  warm {warm:?}  d={first}");
        assert!(
            warm * 50 < cold,
            "a warm query costs {warm:?} against a cold {cold:?} — the mesh is being \
             re-extracted per query, not cached"
        );
    }

    /// Every face of the mesh the renderer draws is on the collider's surface.
    ///
    /// Stated against the triangles rather than against a number, because the
    /// field and the extracted mesh are two different surfaces: surface nets
    /// puts its vertex at the mean of a cell's edge crossings and joins them
    /// flat, which chords inside a bulge and outside a hollow.
    ///
    /// The control is the same points measured against the raw field. If those
    /// also came out at zero there would be no disagreement to fix, so the
    /// disagreement is asserted too. Over the face centres of this fixture at
    /// a 0.5 voxel the field is off the drawn surface by 0.015 on average and
    /// 0.107 at worst, a fifth of a voxel, and it scales with the voxel. Smooth
    /// ground is an order of magnitude better (0.008 mean, 0.014 worst), so
    /// the fixture is the rough one: detail near the voxel size is where
    /// surface nets chords hardest, and it is what a landscape is.
    ///
    /// The same ground at a coarse 1.5 voxel is what a real project uses. A
    /// fixture at 0.5 has a gap the side test steps clean over in either
    /// design; at 1.5 the gap is a quarter of a unit and the designs come
    /// apart.
    fn coarse() -> ChunkField {
        let mut f = ChunkField::new(1.5);
        for _ in 0..30 {
            f.sculpt(Brush::Raise, Vec3::ZERO, 18.0, 1.0, BrushProfile::default());
        }
        let mut seed = 777u32;
        let mut rnd = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as f32 / 16777216.0
        };
        for _ in 0..40 {
            let c = Vec3::new(rnd() * 30.0 - 15.0, rnd() * 12.0 - 3.0, rnd() * 30.0 - 15.0);
            let b = if rnd() > 0.5 { Brush::Raise } else { Brush::Lower };
            f.sculpt(b, c, 3.0 + rnd() * 4.5, 1.0, BrushProfile::default());
        }
        f
    }

    #[test]
    fn a_body_touches_the_terrain_exactly_where_it_is_drawn() {
        let f = coarse();
        // Probe the interior of each drawn triangle, not its corners. A surface
        // nets vertex sits at the mean of its cell's edge crossings and is
        // therefore close to the isosurface almost by construction — the field
        // and the mesh agree there to about three thousandths of a voxel, and a
        // test that sampled corners would conclude there was nothing wrong. The
        // error is the chord: the flat triangle spanning three such vertices
        // cuts inside a bulge and outside a hollow, and the middle of the face
        // is where it is worst. That is also exactly where a player stands.
        // Each probe carries the face's outward normal, for the side test.
        let mut probes = Vec::new();
        for c in f.all_chunk_coords() {
            let m = floptle_field::mesh_chunk(&f, c, 1, false);
            let o = Vec3::from(m.origin);
            for t in m.indices.as_chunks::<3>().0 {
                let v = |i: usize| o + Vec3::from(m.positions[t[i] as usize]);
                let n = |i: usize| Vec3::from(m.normals[t[i] as usize]);
                let centre = (v(0) + v(1) + v(2)) / 3.0;
                let normal = ((n(0) + n(1) + n(2)) / 3.0).normalize_or_zero();
                probes.push((centre, normal));
            }
        }
        assert!(probes.len() > 200, "the hill did not mesh: {} faces", probes.len());

        let t = ChunkTerrain::new(f.clone());
        let mut worst_mesh = 0.0f32;
        let mut worst_field = 0.0f32;
        let mut wrong_side = 0usize;
        for (p, n) in &probes {
            worst_mesh = worst_mesh.max(t.distance(*p).abs());
            worst_field = worst_field.max(f.d(*p).abs());
            // **The side, not just the size.** A step off the face along its
            // normal must read as outside, and a step against it as inside —
            // this is what the field-vs-mesh disagreement actually is, and the
            // half a `.abs()` cannot see. The first cut of this collider took
            // its magnitude from the triangles and its sign from the field,
            // and this assertion written with `.abs()` alone passed on it
            // while physics still settled on the field: the zero crossing is
            // wherever the sign flips, and the sign was still the field's.
            // A tenth of a voxel: well inside the gap between the two surfaces
            // on this ground, and well outside the closest-point solve's noise.
            let step = 0.1 * f.voxel();
            let (out, inn) = (t.distance(*p + *n * step), t.distance(*p - *n * step));
            if out <= 0.0 || inn >= 0.0 {
                wrong_side += 1;
            }
        }
        // A point on the drawn mesh is on the drawn mesh. What is left is the
        // closest-point solve's own rounding.
        assert!(
            worst_mesh < 1e-3,
            "a point ON the drawn surface was {worst_mesh} from the collider's"
        );
        // Not zero: sculpted ground has the odd sliver where a step this size
        // crosses a thin feature and both sides honestly read as inside.
        // Measured: **0.2%** of faces here. The design this replaced — sign
        // from the field — scores **7.2%** on the same ground, and the
        // collider with the mesh switched off fails the magnitude assertion
        // above at 0.31 of a unit. Both were watched red; 2% sits between
        // with room on each side.
        let frac = wrong_side as f32 / probes.len() as f32;
        assert!(
            frac < 0.02,
            "{wrong_side} of {} faces ({:.1}%) have their inside and outside decided by \
             something other than the drawn surface",
            probes.len(),
            frac * 100.0
        );
        // …and the control: the field genuinely disagrees about those same
        // points, by a meaningful fraction of a voxel. Without this the test
        // passes just as well on a collider that never changed.
        assert!(
            worst_field > 0.1,
            "the field agreed with the mesh to {worst_field} of a unit — nothing to reconcile, \
             so this test cannot tell the two colliders apart"
        );
    }

    /// The wireframe the editor draws for the collision surface is built from
    /// `drawn_surface`; the collider meshes chunk by chunk. They share one
    /// extraction, and this holds them to it: every drawn triangle's centre
    /// reads as on the collider, and the field disagrees with most of them, so
    /// a wireframe that lies on the picture is proof the collider does.
    #[test]
    fn the_drawn_surface_is_the_collision_surface() {
        let field = coarse();
        let t = ChunkTerrain::new(field.clone());
        let tris = drawn_surface(&field);
        assert!(tris.len() > 100, "the fixture must have a real surface: {}", tris.len());
        let mut off_field = 0;
        for tri in &tris {
            let c = (tri[0] + tri[1] + tri[2]) / 3.0;
            let d = t.distance(c);
            assert!(d.abs() < 1e-3, "a drawn triangle's centre is on the collider, got {d}");
            if field.d(c).abs() > 0.01 {
                off_field += 1;
            }
        }
        assert!(
            off_field * 2 > tris.len(),
            "the field must disagree with most drawn triangles or this proves nothing: {off_field} of {}",
            tris.len()
        );
        // And the switch: the same terrain set to the field is the field.
        let mut f = ChunkTerrain::new(field.clone());
        f.mesh_accurate = false;
        let c = (tris[0][0] + tris[0][1] + tris[0][2]) / 3.0;
        assert!((f.distance(c) - field.d(c)).abs() < 1e-6, "mesh_accurate = false is the field");
    }

    /// A ray lands on the drawn ground too. A character controller finds its
    /// floor with a raycast far more often than with a contact, and a ray that
    /// stopped on the field while the body stood on the mesh would put the feet
    /// and the ground check a voxel apart.
    ///
    /// Cast straight down from well above the ground, in the sim's own world
    /// frame (rotated, scaled ×5), and compare the hit against the first drawn
    /// triangle on that line, found by an independent ray/triangle intersection
    /// over the extracted mesh so the collider is not grading itself. The trace
    /// stops within 0.02 of the surface from above, so that is the tolerance,
    /// plus a little for the tilt of a face over that last step.
    #[test]
    fn a_ray_lands_on_the_drawn_ground() {
        let f = coarse();
        let scale = 5.0f32;
        let rot = Quat::from_rotation_y(0.3);
        let anchor = Vec3::new(3.0, -7.0, 11.0);
        let t = ChunkTerrain::posed(f.clone(), rot, scale);
        let mut colliders = vec![crate::AnchoredCollider::world(Box::new(t))];
        // `re_anchor`, not a write to `.anchor`: the query offset is cached
        // from it, and a bare field write leaves the collider at the origin.
        colliders[0].re_anchor(anchor.as_dvec3(), floptle_core::math::DVec3::ZERO);

        // Every drawn triangle, field space.
        let mut tris: Vec<[Vec3; 3]> = Vec::new();
        for c in f.all_chunk_coords() {
            let m = floptle_field::mesh_chunk(&f, c, 1, false);
            let o = Vec3::from(m.origin);
            for tri in m.indices.as_chunks::<3>().0 {
                let v = |i: usize| o + Vec3::from(m.positions[tri[i] as usize]);
                tris.push([v(0), v(1), v(2)]);
            }
        }
        // The topmost drawn surface on the vertical through (x, z), field space.
        let drawn_top = |x: f32, z: f32| -> Option<f32> {
            let (ro, rd) = (Vec3::new(x, 1e3, z), Vec3::NEG_Y);
            let mut best: Option<f32> = None;
            for [a, b, c] in &tris {
                let (e1, e2) = (*b - *a, *c - *a);
                let pv = rd.cross(e2);
                let det = e1.dot(pv);
                if det.abs() < 1e-9 {
                    continue;
                }
                let inv = 1.0 / det;
                let sv = ro - *a;
                let u = sv.dot(pv) * inv;
                if !(0.0..=1.0).contains(&u) {
                    continue;
                }
                let qv = sv.cross(e1);
                let v = rd.dot(qv) * inv;
                if v < 0.0 || u + v > 1.0 {
                    continue;
                }
                let tt = e2.dot(qv) * inv;
                if tt <= 0.0 {
                    continue;
                }
                let y = ro.y - tt;
                best = Some(best.map_or(y, |b: f32| b.max(y)));
            }
            best
        };

        let mut worst = 0.0f32;
        let (mut cast, mut missed) = (0usize, 0usize);
        for ix in -14..=14 {
            for iz in -14..=14 {
                let (x, z) = (ix as f32 * 1.1, iz as f32 * 1.1);
                let Some(top) = drawn_top(x, z) else { continue };
                cast += 1;
                // Start 30 field units up — above every stamp in the fixture
                // — and cast straight down the field's own vertical, which is
                // a tilted line in the sim frame because the terrain is rotated.
                let from = anchor + rot * (Vec3::new(x, top + 30.0, z) * scale);
                let dir = rot * Vec3::NEG_Y;
                match crate::raycast_colliders(&colliders, from, dir, 400.0, !0) {
                    Some(hit) => {
                        let local = rot.inverse() * (Vec3::from(hit.point) - anchor) / scale;
                        worst = worst.max((local.y - top).abs() * scale);
                    }
                    None => missed += 1,
                }
            }
        }
        assert!(cast > 400, "{cast} rays cast");
        assert_eq!(missed, 0, "{missed} of {cast} rays fell through the ground");
        assert!(
            worst < 0.08,
            "a ray cast at the drawn ground stopped {worst} world units off it"
        );
    }

    /// The sign still comes from the field, so a cave is still hollow.
    ///
    /// Triangle distance alone is unsigned — that is why [`TriMeshCollider`]
    /// cannot do this job — and a collider that lost the sign would report a
    /// point in mid-air inside a tunnel as being in solid rock, which is a body
    /// stuck in a wall rather than a body walking through it.
    #[test]
    fn the_drawn_surface_collider_still_knows_inside_from_outside() {
        let mut f = hill();
        // Bore a tunnel through the hill just under its crown.
        for _ in 0..30 {
            f.sculpt(Brush::Lower, Vec3::new(0.0, 1.0, 0.0), 2.0, 1.0, BrushProfile::default());
        }
        let t = ChunkTerrain::new(f.clone());
        let air = Vec3::new(0.0, 1.0, 0.0); // in the bore
        let rock = Vec3::new(0.0, -3.0, 0.0); // under it
        assert!(t.distance(air) > 0.0, "the tunnel read as solid: {}", t.distance(air));
        assert!(t.distance(rock) < 0.0, "the rock read as air: {}", t.distance(rock));
    }

    /// A dig drops the cached triangles, and the cache really was holding the
    /// old ground up until it did.
    ///
    /// The cache is the only thing that could hold a dug tunnel shut, so the
    /// order matters: warm, dig with the cache left alone and assert the answer
    /// is wrong, then invalidate and assert it comes right. Invalidating before
    /// the dig would prove nothing, since the "stale" reading would be freshly
    /// meshed.
    ///
    /// `Sim::terrain_field_mut` calls this in earnest, and
    /// `digging_under_a_body_updates_collision` tells the same story through a
    /// falling body.
    #[test]
    fn writing_the_field_forgets_the_cached_surface() {
        let mut t = ChunkTerrain::new(hill());
        // Just above the crown, inside the narrow band — outside it the collider
        // short-circuits to the field and never touches a triangle, so a probe
        // in open air would test nothing. Found rather than assumed: the height
        // a stack of Raise brushes settles at is not worth hard-coding.
        let top = (0..400)
            .map(|i| 8.0 - i as f32 * 0.05)
            .find(|y| t.field.d(Vec3::new(0.0, *y, 0.0)) <= 0.0)
            .expect("the hill has a surface");
        let probe = Vec3::new(0.0, top + t.field.voxel(), 0.0);
        let before = t.distance(probe); // warms the cache at the probe
        assert!(
            before.abs() < t.field.band(),
            "the probe is outside the band at {before}; the collider never meshes there"
        );
        // Carve straight down from under the probe, hard enough that the drawn
        // surface visibly drops away from it.
        for _ in 0..40 {
            t.field.sculpt(
                Brush::Lower,
                probe - Vec3::Y * 0.5,
                3.0,
                1.0,
                BrushProfile::default(),
            );
        }
        assert!(
            t.field.d(probe) > before + 0.25,
            "the fixture's dig did not move the field under the probe: {before} -> {}",
            t.field.d(probe)
        );
        let stale = t.distance(probe);
        assert!(
            (stale - before).abs() < 1e-4,
            "the cache was never consulted, so nothing here tests dropping it: \
             {before} -> {stale}"
        );
        t.invalidate_surface();
        let fresh = t.distance(probe);
        assert!(
            fresh > before + 0.1,
            "digging under the probe did not open ground beneath it: {before} -> {fresh}"
        );
    }
}
