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
    /// [`Self::distance`], asked only whether it is under `r`.
    ///
    /// Where `distance(p) < r` this returns exactly `distance(p)`, bit for
    /// bit; anywhere else it returns some value at least `r`. A contact needs
    /// nothing more: a sample is touching when its distance is under its
    /// radius, and how far beyond that the nearest face is changes nothing.
    /// A shape whose search can stop at `r` overrides this; the default asks
    /// the whole question.
    fn distance_within(&self, p: Vec3, r: f32) -> f32 {
        let _ = r;
        self.distance(p)
    }
    /// Whether [`Self::distance`] is at least `r` at every point of the box
    /// `lo ..= hi`, known without asking point by point.
    ///
    /// `true` is a promise; `false` means only "cannot tell cheaply", and is
    /// the default. What lets a shape with no bounds of its own (a terrain
    /// field) be skipped for the parts of a craft in open air above it.
    fn clear_of(&self, lo: Vec3, hi: Vec3, r: f32) -> bool {
        let _ = (lo, hi, r);
        false
    }
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
    /// The normal a ray reports where it stopped at `p`, travelling along
    /// unit `rd`.
    ///
    /// A ray that meets a face head-on stops on it, and a normal taken from
    /// the closest point there is the direction of a vector a few ulps long.
    /// A shape that answers from its closest point overrides this with the
    /// face the ray crossed. The default is `normal(p)`, which an analytic
    /// shape answers in closed form wherever it is asked.
    fn ray_normal(&self, p: Vec3, rd: Vec3) -> Vec3 {
        let _ = rd;
        self.normal(p)
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
    /// An axis-aligned box `(min, max)` such that [`Self::distance`] at any
    /// point is at least that point's distance to the box.
    ///
    /// Tighter than [`Self::bounds`] for anything long and thin — a 50 m
    /// building's sphere is mostly empty air — and what lets a body skip a
    /// shape it is nowhere near without asking it. `None` (the default) means
    /// no such box, and the shape is always tested.
    fn aabb(&self) -> Option<(Vec3, Vec3)> {
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
    /// An exact answer to "where does the ray from `o` along unit `rd` first
    /// meet this shape, within `tmax`", when the shape has one cheaper than a
    /// sphere march. [`RayAnswer::March`] (the default) hands the ray to the
    /// march in `raycast_colliders`.
    fn ray_cast(&self, o: Vec3, rd: Vec3, tmax: f32) -> RayAnswer {
        let _ = (o, rd, tmax);
        RayAnswer::March
    }
    /// [`Self::ray_cast`] for a question asked thousands of times over new
    /// ground (dropping scattered props onto it): the same answer, by a way
    /// that never does lasting work such as meshing a terrain chunk for
    /// collision. The default is the ordinary ray.
    fn ground_cast(&self, o: Vec3, rd: Vec3, tmax: f32) -> RayAnswer {
        self.ray_cast(o, rd, tmax)
    }
    /// Push every face of this shape that may cross the box `(lo, hi)` onto
    /// `out`, in the shape's own frame, and say whether the shape can answer at
    /// all. A projected decal lays itself on what comes back.
    ///
    /// Conservative: a face near the box may come back too, and the caller
    /// clips. `false` (the default) is a shape with no faces to give: a sphere,
    /// a capsule, a baked SDF.
    fn triangles_in(&self, lo: Vec3, hi: Vec3, out: &mut Vec<FaceTri>) -> bool {
        let _ = (lo, hi, out);
        false
    }
}

/// One face of a collider as [`CollisionShape::triangles_in`] hands it out:
/// its corners, and the normal at each. A flat face has the same normal three
/// times; a terrain face carries the drawn surface's own vertex normals, so
/// what is laid on it is lit the way the ground under it is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FaceTri {
    pub p: [Vec3; 3],
    pub n: [Vec3; 3],
}

impl FaceTri {
    /// A flat face, its normal from its winding (counter-clockwise = front).
    pub fn flat(p: [Vec3; 3]) -> Self {
        let n = (p[1] - p[0]).cross(p[2] - p[0]).normalize_or_zero();
        Self { p, n: [n; 3] }
    }
}

/// Does the triangle's box meet `(lo, hi)`?
fn tri_box_overlap(t: &[Vec3; 3], lo: Vec3, hi: Vec3) -> bool {
    let (a, b) = (t[0].min(t[1]).min(t[2]), t[0].max(t[1]).max(t[2]));
    a.cmple(hi).all() && b.cmpge(lo).all()
}

/// What [`CollisionShape::ray_cast`] knows.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RayAnswer {
    /// Nothing exact: march it.
    March,
    Miss,
    /// Distance along the ray, and the face's normal facing back along it.
    Hit(f32, Vec3),
}

/// Where the ray `o + rd·t` is inside the box `(lo, hi)`, clipped to
/// `[0, tmax]`: the slab test. `None` when it never is.
pub fn ray_box_span(o: Vec3, rd: Vec3, lo: Vec3, hi: Vec3, tmax: f32) -> Option<(f32, f32)> {
    let (mut t0, mut t1) = (0.0f32, tmax);
    for k in 0..3 {
        if rd[k].abs() < 1e-12 {
            if o[k] < lo[k] || o[k] > hi[k] {
                return None;
            }
            continue;
        }
        let inv = 1.0 / rd[k];
        let (a, b) = ((lo[k] - o[k]) * inv, (hi[k] - o[k]) * inv);
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        t0 = t0.max(a);
        t1 = t1.min(b);
        if t0 > t1 {
            return None;
        }
    }
    Some((t0, t1))
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
    /// A square of the plane big enough to cover the box, when the plane
    /// passes through it.
    fn triangles_in(&self, lo: Vec3, hi: Vec3, out: &mut Vec<FaceTri>) -> bool {
        let n = self.normal.try_normalize().unwrap_or(Vec3::Y);
        let (c, half) = ((lo + hi) * 0.5, (hi - lo) * 0.5);
        let d = (c - self.point).dot(n);
        if d.abs() > half.dot(n.abs()) {
            return true;
        }
        let on = c - n * d;
        let u = n.any_orthonormal_vector();
        let v = n.cross(u);
        let r = half.length() * 1.01;
        let q = [on - u * r - v * r, on + u * r - v * r, on + u * r + v * r, on - u * r + v * r];
        out.push(FaceTri { p: [q[0], q[2], q[1]], n: [n; 3] }.front(n));
        out.push(FaceTri { p: [q[0], q[3], q[2]], n: [n; 3] }.front(n));
        true
    }
}

impl FaceTri {
    /// Wound so its front faces `n`.
    fn front(self, n: Vec3) -> Self {
        let w = (self.p[1] - self.p[0]).cross(self.p[2] - self.p[0]);
        if w.dot(n) < 0.0 {
            Self { p: [self.p[0], self.p[2], self.p[1]], ..self }
        } else {
            self
        }
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
    /// Its six faces, two triangles each.
    fn triangles_in(&self, lo: Vec3, hi: Vec3, out: &mut Vec<FaceTri>) -> bool {
        let (c, r) = (self.center, self.half.length());
        if (c - Vec3::splat(r)).cmpgt(hi).any() || (c + Vec3::splat(r)).cmplt(lo).any() {
            return true;
        }
        let rot = self.inv_rot.inverse();
        let h = self.half;
        for axis in 0..3 {
            for sign in [-1.0f32, 1.0] {
                let mut n = Vec3::ZERO;
                n[axis] = sign;
                let (a, b) = ((axis + 1) % 3, (axis + 2) % 3);
                let corner = |su: f32, sv: f32| {
                    let mut l = n * h;
                    l[a] = su * h[a];
                    l[b] = sv * h[b];
                    c + rot * l
                };
                let q = [corner(-1.0, -1.0), corner(1.0, -1.0), corner(1.0, 1.0), corner(-1.0, 1.0)];
                let wn = rot * n;
                for t in [[q[0], q[1], q[2]], [q[0], q[2], q[3]]] {
                    if tri_box_overlap(&t, lo, hi) {
                        out.push(FaceTri { p: t, n: [wn; 3] }.front(wn));
                    }
                }
            }
        }
        true
    }
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
    /// Every meshed chunk's triangles, an empty one included: a chunk of pure
    /// air or pure rock extracts nothing and belongs in here exactly as much as
    /// one that extracted a thousand triangles, so it is not re-meshed every
    /// query.
    chunks: CellMap<[i32; 3], CollisionChunk>,
    /// How many rays found the drawn surface in each chunk not meshed yet,
    /// from a few cells meshed for the asking. A chunk asked about often is
    /// worth meshing whole.
    asked: std::collections::HashMap<[i32; 3], u8>,
    /// Chunks meshed by a query, on the thread that asked: what a cast near
    /// ground nobody had meshed pays for. Meshes handed in are not counted.
    meshed_by_queries: u64,
    /// Chunks meshed ahead of a mover, off the thread that steps: see
    /// [`ChunkTerrain::mesh_ahead`].
    #[cfg(not(target_arch = "wasm32"))]
    ahead: MeshAhead,
}

/// A chunk to mesh ahead: `(epoch, chunk, its voxels, bucket size)`.
#[cfg(not(target_arch = "wasm32"))]
type AheadJob = (u64, [i32; 3], floptle_field::MeshScratch, f32);
/// A chunk meshed ahead: `(epoch, chunk, its triangles)`.
#[cfg(not(target_arch = "wasm32"))]
type AheadDone = (u64, [i32; 3], CollisionChunk);

/// Collision meshes being made off the stepping thread for ground a mover is
/// heading for, and when each lands. See [`ChunkTerrain::mesh_ahead`].
#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
struct MeshAhead {
    /// The worker's job and answer channels, once anything has been asked.
    worker: Option<(std::sync::mpsc::Sender<AheadJob>, std::sync::mpsc::Receiver<AheadDone>)>,
    /// Asked for and not landed yet, in the order asked: `(step it lands
    /// on, epoch, chunk)`.
    queued: std::collections::VecDeque<(u64, u64, [i32; 3])>,
    /// The chunks in `queued`.
    queued_set: std::collections::HashSet<[i32; 3]>,
    /// Answers in, waiting for their step.
    ready: std::collections::HashMap<[i32; 3], (u64, CollisionChunk)>,
    /// Steps counted by [`ChunkTerrain::mesh_ahead`].
    step: u64,
    /// Bumped whenever cached triangles are forgotten: an answer made from
    /// the field before is dropped.
    epoch: u64,
    /// Chunks landed this way. Test-only reads.
    landed: u64,
}

/// The meshed chunks around one query, looked up once rather than once per
/// cell: up to two chunks a side, which covers every cell a query within a
/// chunk's width can visit. `None` when the box needs more, and the caller
/// asks [`Surface::each_in`] instead.
struct NearChunks<'a> {
    lo: [i32; 3],
    table: [Option<&'a CollisionChunk>; 8],
    /// Every bucket any of them holds triangles in is inside this range.
    occupied: ((i32, i32, i32), (i32, i32, i32)),
}

impl<'a> NearChunks<'a> {
    fn new(s: &'a Surface, lo_cell: (i32, i32, i32), hi_cell: (i32, i32, i32)) -> Option<Self> {
        let n = floptle_field::CHUNK;
        let lo = [lo_cell.0, lo_cell.1, lo_cell.2].map(|k| (k - 2).div_euclid(n));
        let hi = [hi_cell.0, hi_cell.1, hi_cell.2].map(|k| (k + 2).div_euclid(n));
        if (0..3).any(|i| hi[i] - lo[i] > 1) {
            return None;
        }
        let mut table = [None; 8];
        let mut occupied = NO_CELLS;
        for z in lo[2]..=hi[2] {
            for y in lo[1]..=hi[1] {
                for x in lo[0]..=hi[0] {
                    let ch = s.chunks.get(&[x, y, z]);
                    if let Some(ch) = ch {
                        let ((a, b), (c, d)) = (occupied, ch.occupied);
                        occupied = ((a.0.min(c.0), a.1.min(c.1), a.2.min(c.2)), (b.0.max(d.0), b.1.max(d.1), b.2.max(d.2)));
                    }
                    table[Self::at(lo, x, y, z)] = ch;
                }
            }
        }
        Some(Self { lo, table, occupied })
    }

    #[inline]
    fn at(lo: [i32; 3], x: i32, y: i32, z: i32) -> usize {
        ((x - lo[0]) + 2 * ((y - lo[1]) + 2 * (z - lo[2]))) as usize
    }

    /// [`Surface::each_in`], from the table: the same chunks in the same
    /// order, so the same triangles in the same order.
    #[inline]
    fn each_in(&self, cell: (i32, i32, i32), mut f: impl FnMut(&SurfaceTri)) {
        let span = |k: i32| ((k - 2).div_euclid(floptle_field::CHUNK), (k + 2).div_euclid(floptle_field::CHUNK));
        let ((x0, x1), (y0, y1), (z0, z1)) = (span(cell.0), span(cell.1), span(cell.2));
        for z in z0..=z1 {
            for y in y0..=y1 {
                for x in x0..=x1 {
                    if let Some(ch) = self.table[Self::at(self.lo, x, y, z)] {
                        ch.cell(cell).iter().for_each(&mut f);
                    }
                }
            }
        }
    }
}

impl Surface {
    /// Every triangle bucketed in `cell`, from whichever chunks reach it.
    ///
    /// A chunk's triangles reach a voxel past its own edges (its mesh takes a
    /// layer from each neighbour), so a cell within two voxels of a chunk face
    /// asks the chunk across it too; any other asks one.
    fn each_in(&self, cell: (i32, i32, i32), mut f: impl FnMut(&SurfaceTri)) {
        let span = |k: i32| ((k - 2).div_euclid(floptle_field::CHUNK), (k + 2).div_euclid(floptle_field::CHUNK));
        let ((x0, x1), (y0, y1), (z0, z1)) = (span(cell.0), span(cell.1), span(cell.2));
        for z in z0..=z1 {
            for y in y0..=y1 {
                for x in x0..=x1 {
                    if let Some(ch) = self.chunks.get(&[x, y, z]) {
                        ch.cell(cell).iter().for_each(&mut f);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Bounded mesh searches run, and triangles any mesh search tested,
    /// bounded or not. Test-only.
    pub(crate) static MESH_BOUNDED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    pub(crate) static MESH_TRIS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Drawn-surface triangles a terrain query walked, and of those, the ones
    /// it tested rather than skipped by their box. Test-only.
    pub(crate) static TERRAIN_VISITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    pub(crate) static TERRAIN_TRIS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Buckets a terrain query looked up. Test-only.
    pub(crate) static TERRAIN_BUCKETS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Walk every bucket a terrain distance reaches, as it did before far
    /// buckets were skipped: the reference that walk is compared with.
    /// Test-only.
    pub(crate) static TERRAIN_EVERY_BUCKET: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A hasher for small integer keys (cell and chunk coordinates).
///
/// A query near the ground looks a bucket up in every cell it visits, and the
/// standard hasher spent more on each lookup than the triangle tests did. A key
/// is a few `i32`s, chosen by geometry and never by an adversary, so a
/// multiply-rotate mix is enough. Only lookups go through it; nothing iterates
/// these maps in an order that could change an answer.
#[derive(Default, Clone, Copy)]
struct CellHasher(u64);

impl std::hash::Hasher for CellHasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for c in bytes.chunks(8) {
            let mut b = [0u8; 8];
            b[..c.len()].copy_from_slice(c);
            self.write_u64(u64::from_le_bytes(b));
        }
    }
    #[inline]
    fn write_u64(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
    #[inline]
    fn write_u32(&mut self, v: u32) {
        self.write_u64(v as u64);
    }
    #[inline]
    fn write_i32(&mut self, v: i32) {
        self.write_u64(v as u32 as u64);
    }
    #[inline]
    fn write_usize(&mut self, v: usize) {
        self.write_u64(v as u64);
    }
}

type CellMap<K, V> = std::collections::HashMap<K, V, std::hash::BuildHasherDefault<CellHasher>>;

/// One chunk's collision triangles, bucketed for closest-point queries: one
/// list sorted by bucket, each bucket a run of it. Two allocations a chunk,
/// whatever its size, so building one off the main thread and handing it over
/// costs the main thread nothing to speak of; a bucket each was thousands of
/// small allocations, a few milliseconds a chunk, every time a dig re-meshed.
#[derive(Clone)]
pub struct CollisionChunk {
    runs: CellMap<(i32, i32, i32), (u32, u32)>,
    tris: Vec<SurfaceTri>,
    /// The inclusive range of buckets that hold anything, so a query can
    /// leave the air above a floor without asking about each bucket in it.
    /// Inverted (`lo > hi`) when the chunk holds nothing.
    occupied: ((i32, i32, i32), (i32, i32, i32)),
}

/// The empty bucket range: every `min`/`max` folded into it gives the other.
const NO_CELLS: ((i32, i32, i32), (i32, i32, i32)) = ((i32::MAX, i32::MAX, i32::MAX), (i32::MIN, i32::MIN, i32::MIN));

impl Default for CollisionChunk {
    fn default() -> Self {
        Self { runs: CellMap::default(), tris: Vec::new(), occupied: NO_CELLS }
    }
}

impl CollisionChunk {
    /// Bucket a chunk's triangles from its mesh `m` (a skirt, if it has one,
    /// left out), in buckets `cell` across: the voxel of the field it was
    /// meshed from.
    pub fn from_mesh(m: &floptle_field::ChunkMesh, cell: f32) -> Self {
        let origin = Vec3::from(m.origin);
        let surface = m.indices.len() - m.skirt_tris * 3;
        let mut placed: Vec<((i32, i32, i32), SurfaceTri)> = Vec::new();
        for t in m.indices[..surface].as_chunks::<3>().0 {
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
            let nrm = |i: u32| Vec3::from(m.normals[i as usize]).try_normalize().unwrap_or(face);
            let entry = SurfaceTri { p: tri, n: [nrm(t[0]), nrm(t[1]), nrm(t[2])] };
            let lo = cell_coord(tri[0].min(tri[1]).min(tri[2]), cell);
            let hi = cell_coord(tri[0].max(tri[1]).max(tri[2]), cell);
            for cz in lo.2..=hi.2 {
                for cy in lo.1..=hi.1 {
                    for cx in lo.0..=hi.0 {
                        placed.push(((cx, cy, cz), entry));
                    }
                }
            }
        }
        placed.sort_by_key(|(k, _)| *k);
        let mut runs = CellMap::default();
        let mut tris = Vec::with_capacity(placed.len());
        let mut i = 0;
        while i < placed.len() {
            let k = placed[i].0;
            let start = tris.len() as u32;
            while i < placed.len() && placed[i].0 == k {
                tris.push(placed[i].1);
                i += 1;
            }
            runs.insert(k, (start, tris.len() as u32 - start));
        }
        let occupied = runs.keys().fold(NO_CELLS, |(lo, hi), k| {
            ((lo.0.min(k.0), lo.1.min(k.1), lo.2.min(k.2)), (hi.0.max(k.0), hi.1.max(k.1), hi.2.max(k.2)))
        });
        Self { runs, tris, occupied }
    }

    fn cell(&self, k: (i32, i32, i32)) -> &[SurfaceTri] {
        match self.runs.get(&k) {
            Some(&(start, n)) => &self.tris[start as usize..(start + n) as usize],
            None => &[],
        }
    }
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

    /// The sphere around chunk `c`, anchor-relative like every query, grown
    /// by one voxel: a re-mesh moves the surface up to the chunk's border and
    /// a voxel past it.
    pub fn chunk_sphere(&self, c: [i32; 3]) -> (Vec3, f32) {
        let v = self.field.voxel();
        let side = floptle_field::CHUNK as f32 * v;
        let centre = self.field.chunk_origin(c) + Vec3::splat(side * 0.5);
        let s = self.scale.max(1e-6);
        (self.rot * (centre * s), (side * 0.5 * 3f32.sqrt() + v) * s)
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
        let s = self.surface.get_mut();
        s.chunks.clear();
        #[cfg(not(target_arch = "wasm32"))]
        {
            s.ahead.epoch += 1;
        }
    }

    /// **Forget the meshed chunks an edit of `written` could have changed**:
    /// those chunks and every chunk next to them, since a chunk's mesh reads a
    /// layer into its neighbours. A dig re-meshes the few chunks it reached on
    /// the next query near them, not every chunk any body was standing in.
    pub fn invalidate_chunks(&mut self, written: &[[i32; 3]]) {
        let mut ring: Vec<[i32; 3]> = Vec::new();
        for c in written {
            for dz in -1..=1 {
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        ring.push([c[0] + dx, c[1] + dy, c[2] + dz]);
                    }
                }
            }
        }
        self.invalidate_meshes(&ring);
    }

    /// **Forget exactly these chunks' meshed triangles**, for a caller that
    /// knows which meshes an edit reached (`ChunkField::chunks_reading` over
    /// the box it changed). Costs the cells those chunks filled, not the cache:
    /// a sweep of every cached triangle per dig was most of a mining laser's
    /// frame on a planet a player had walked across.
    /// **Take chunk `c`'s collision mesh as already built**, in place of the one
    /// cached. Meshed off the main thread at full detail from this collider's
    /// own field, so the main thread pays for bucketing the triangles, not for
    /// extracting them; until it arrives, the old triangles stand, as the old
    /// mesh stays drawn until its replacement lands.
    /// Is chunk `c`'s collision mesh cached?
    pub fn is_meshed(&self, c: [i32; 3]) -> bool {
        self.surface.borrow().chunks.contains_key(&c)
    }

    /// The chunks a gather of the faces in the box `lo..hi` (anchor-relative,
    /// as a query's) would mesh on the spot, not meshed yet: the ones to hand
    /// a worker first, so laying a mark there costs the frame no meshing.
    pub fn unmeshed_in_box(&self, lo: Vec3, hi: Vec3) -> Vec<[i32; 3]> {
        let (llo, lhi) = self.local_box(lo, hi);
        let s = self.surface.borrow();
        self.field.chunks_in_world_box(llo, lhi).into_iter().filter(|c| !s.chunks.contains_key(c)).collect()
    }

    /// An anchor-relative box, boxed again in the field's own frame.
    fn local_box(&self, lo: Vec3, hi: Vec3) -> (Vec3, Vec3) {
        let scale = self.scale.max(1e-6);
        let (c, half) = ((lo + hi) * 0.5, (hi - lo) * 0.5);
        let m = floptle_core::math::Mat3::from_quat(self.rot.inverse());
        let half_l = (m.x_axis.abs() * half.x + m.y_axis.abs() * half.y + m.z_axis.abs() * half.z) / scale;
        let c_l = self.to_local(c);
        (c_l - half_l, c_l + half_l)
    }

    /// How many chunks' collision meshes are cached.
    pub fn meshed_chunks(&self) -> usize {
        self.surface.borrow().chunks.len()
    }

    /// How many chunks a query has had to mesh itself, on the thread that
    /// asked, since this collider was made: the cost of the first cast at
    /// ground no worker meshed (see [`Self::adopt`]).
    pub fn meshed_by_queries(&self) -> u64 {
        self.surface.borrow().meshed_by_queries
    }

    pub fn adopt_mesh(&mut self, c: [i32; 3], m: &floptle_field::ChunkMesh) {
        let built = CollisionChunk::from_mesh(m, self.bucket());
        self.adopt(c, built);
    }

    /// [`Self::adopt_mesh`] with the bucketing done too, where the mesh was
    /// built ([`CollisionChunk::from_mesh`] with this field's voxel).
    pub fn adopt(&mut self, c: [i32; 3], built: CollisionChunk) {
        self.surface.get_mut().chunks.insert(c, built);
    }

    pub fn invalidate_meshes(&mut self, stale: &[[i32; 3]]) {
        let s = self.surface.get_mut();
        for c in stale {
            s.chunks.remove(c);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            s.ahead.epoch += 1;
        }
    }

    /// How many steps after it is asked for a chunk meshed ahead lands.
    #[cfg(not(target_arch = "wasm32"))]
    const AHEAD_STEPS: u64 = 12;

    /// Mesh, off the thread that steps, the chunks of ground a mover is
    /// heading for: the ones a query anywhere in the box `lo..hi` (this
    /// collider's frame, as a query's point is) would mesh. Called once a
    /// step, with every box, empty included, since it is also what lands the
    /// chunks asked for before.
    ///
    /// A query that reaches a chunk no one has meshed meshes it on the spot,
    /// whole, which on detailed ground is tens of milliseconds a chunk, and a
    /// craft crossing a corner meets four at once: a fast plane's sweep paid
    /// for its ground that way. Asked for here half a second ahead, a chunk
    /// is meshed on a worker and lands a fixed number of steps after it was
    /// asked for, waited for if the worker has not finished it by then, so
    /// when a chunk lands depends only on the steps, never on how fast the
    /// worker was. Its triangles are the ones a query would have made, from
    /// the same voxels, and a query that needs a chunk before it lands
    /// meshes it as it did, so what anything collides with is unchanged.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn mesh_ahead(&self, boxes: &[(Vec3, Vec3)]) {
        // At most this many chunks asked for a step, and outstanding.
        const PER_STEP: usize = 16;
        const OUTSTANDING: usize = 128;
        let mut s = self.surface.borrow_mut();
        let s = &mut *s;
        let a = &mut s.ahead;
        a.step += 1;
        if a.worker.is_none() && boxes.is_empty() {
            return;
        }
        // Land what is due, waiting for any still on the worker.
        while let Some(&(due, epoch, c)) = a.queued.front() {
            if due > a.step {
                break;
            }
            a.queued.pop_front();
            a.queued_set.remove(&c);
            let mut answer = a.ready.remove(&c);
            if answer.as_ref().is_none_or(|(e, _)| *e != epoch)
                && let Some((_, rx)) = a.worker.as_ref()
            {
                while let Ok((e, k, chunk)) = rx.recv_timeout(std::time::Duration::from_secs(5)) {
                    if k == c && e == epoch {
                        answer = Some((e, chunk));
                        break;
                    }
                    a.ready.insert(k, (e, chunk));
                }
            }
            if let Some((e, chunk)) = answer
                && e == a.epoch
                && !s.chunks.contains_key(&c)
            {
                s.chunks.insert(c, chunk);
                a.landed += 1;
            }
        }
        if let Some((_, rx)) = a.worker.as_ref() {
            while let Ok((e, k, chunk)) = rx.try_recv() {
                a.ready.insert(k, (e, chunk));
            }
        }
        let mut asked = 0;
        for &(lo, hi) in boxes {
            // The box in the field's own frame, grown by how far a query
            // looks, in chunks.
            let corners = [
                Vec3::new(lo.x, lo.y, lo.z),
                Vec3::new(hi.x, lo.y, lo.z),
                Vec3::new(lo.x, hi.y, lo.z),
                Vec3::new(hi.x, hi.y, lo.z),
                Vec3::new(lo.x, lo.y, hi.z),
                Vec3::new(hi.x, lo.y, hi.z),
                Vec3::new(lo.x, hi.y, hi.z),
                Vec3::new(hi.x, hi.y, hi.z),
            ]
            .map(|p| self.to_local(p));
            let r = Vec3::splat(self.reach());
            let llo = corners.iter().fold(Vec3::splat(f32::INFINITY), |m, p| m.min(*p)) - r;
            let lhi = corners.iter().fold(Vec3::splat(f32::NEG_INFINITY), |m, p| m.max(*p)) + r;
            let v = self.field.voxel();
            let n = floptle_field::CHUNK;
            let clo = [(llo.x / v).floor() as i32, (llo.y / v).floor() as i32, (llo.z / v).floor() as i32].map(|i| i.div_euclid(n));
            let chi = [(lhi.x / v).ceil() as i32, (lhi.y / v).ceil() as i32, (lhi.z / v).ceil() as i32].map(|i| i.div_euclid(n));
            // A box this many chunks across is a craft far too fast for the
            // ground to matter (or nowhere near it); asked about, it would
            // queue the planet.
            let count = (0..3).map(|k| (chi[k] - clo[k] + 1).max(0) as i64).product::<i64>();
            if count > 64 {
                continue;
            }
            let side = n as f32 * v;
            for cz in clo[2]..=chi[2] {
                for cy in clo[1]..=chi[1] {
                    for cx in clo[0]..=chi[0] {
                        let c = [cx, cy, cz];
                        if asked >= PER_STEP || a.queued.len() >= OUTSTANDING {
                            return;
                        }
                        if s.chunks.contains_key(&c) || a.queued_set.contains(&c) {
                            continue;
                        }
                        // Open air has no ground to mesh, and no query near
                        // it meshes it either.
                        let o = self.field.chunk_origin(c);
                        if self.field.open_air_in_box(o, o + Vec3::splat(side)) {
                            continue;
                        }
                        let (tx, _) = a.worker.get_or_insert_with(Self::spawn_mesher);
                        let scratch = floptle_field::scratch_for_chunk(&self.field, c, 1);
                        if tx.send((a.epoch, c, scratch, self.bucket())).is_err() {
                            return;
                        }
                        a.queued.push_back((a.step + Self::AHEAD_STEPS, a.epoch, c));
                        a.queued_set.insert(c);
                        asked += 1;
                    }
                }
            }
        }
    }

    /// The worker [`Self::mesh_ahead`] hands chunks to: meshes each the way
    /// a query does ([`drawn_chunk`]) and sends back its collision triangles.
    /// It ends when this collider is dropped.
    #[cfg(not(target_arch = "wasm32"))]
    fn spawn_mesher() -> (std::sync::mpsc::Sender<AheadJob>, std::sync::mpsc::Receiver<AheadDone>) {
        let (jtx, jrx) = std::sync::mpsc::channel::<AheadJob>();
        let (dtx, drx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new().name("terrain-collision-ahead".into()).spawn(move || {
            for (epoch, c, scratch, bucket) in jrx {
                let m = floptle_field::mesh_scratch(&scratch, false);
                if dtx.send((epoch, c, CollisionChunk::from_mesh(&m, bucket))).is_err() {
                    break;
                }
            }
        });
        if spawned.is_err() {
            // No thread to be had: the senders close with this scope, every
            // request fails, and queries mesh their own ground as before.
            let (jtx, _) = std::sync::mpsc::channel();
            let (_, drx) = std::sync::mpsc::channel();
            return (jtx, drx);
        }
        (jtx, drx)
    }

    /// How many chunks [`Self::mesh_ahead`] has landed. Test-only reads.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn meshed_ahead(&self) -> u64 {
        self.surface.borrow().ahead.landed
    }

    /// Make sure every chunk within `cell()` of `local` has been meshed.
    fn ensure_meshed(&self, local: Vec3, s: &mut Surface) {
        self.each_chunk_near(local, |c| {
            self.mesh_into(c, s);
            true
        });
    }

    /// The chunks the drawn surface near `local` is read from — the ones
    /// `ChunkField::chunks_in_world_box` names for the reach round it —
    /// walked in place, while `f` answers true. Every query near the ground
    /// asks this and nearly always finds them all meshed already, so
    /// building the list was most of what it cost.
    fn each_chunk_near(&self, local: Vec3, mut f: impl FnMut([i32; 3]) -> bool) -> bool {
        let r = Vec3::splat(self.reach());
        let (lo, hi) = (local - r, local + r);
        let v = self.field.voxel();
        let n = floptle_field::CHUNK;
        let lo = [(lo.x / v).floor() as i32, (lo.y / v).floor() as i32, (lo.z / v).floor() as i32].map(|i| i.div_euclid(n));
        let hi = [(hi.x / v).ceil() as i32, (hi.y / v).ceil() as i32, (hi.z / v).ceil() as i32].map(|i| i.div_euclid(n));
        for cz in lo[2]..=hi[2] {
            for cy in lo[1]..=hi[1] {
                for cx in lo[0]..=hi[0] {
                    if !f([cx, cy, cz]) {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// The first drawn triangle a ray from `o` along `d` (local, unit) meets
    /// near `t0`, where it first comes within a voxel of the field, and its
    /// face normal toward the ray: the cells along that stretch of it meshed
    /// for the asking, a small box at a time. The drawn surface stands up to
    /// half a voxel off the field's, so a ray near square to the ground can
    /// cross it a little before `t0`, and the walk starts back from there.
    /// `None` when it meets nothing.
    fn hit_drawn_box(&self, o: Vec3, d: Vec3, t0: f32, tmax: f32) -> Option<(f32, Vec3)> {
        const CELLS: i32 = 8;
        // A triangle near a point is meshed in a box when the point is this far
        // inside it: the edge it is built round has its low corner in the box.
        const CORE: f32 = CELLS as f32 * 0.5 - 1.5;
        let voxel = self.field.voxel();
        let mut t = (t0 - 1.5 * voxel).max(0.0);
        while t <= (t0 + 6.0 * voxel).min(tmax) {
            let p = o + d * t;
            let centre = (p / voxel).round();
            let base = [
                centre.x as i32 - CELLS / 2,
                centre.y as i32 - CELLS / 2,
                centre.z as i32 - CELLS / 2,
            ];
            let m = floptle_field::mesh_box(&self.field, base, CELLS);
            let origin = Vec3::from(m.origin);
            let mut best: Option<(f32, Vec3)> = None;
            for tri in m.indices.chunks(3) {
                let [a, b, c] = [0, 1, 2].map(|k| Vec3::from(m.positions[tri[k] as usize]) + origin);
                if let Some(h) = ray_triangle(o, d, a, b, c)
                    && h > 0.0
                    && h <= tmax
                    && ((o + d * h) / voxel - centre).abs().max_element() <= CORE
                    && best.is_none_or(|(bt, _)| h < bt)
                {
                    let n = (b - a).cross(c - a).normalize_or_zero();
                    best = Some((h, if n.dot(d) > 0.0 { -n } else { n }));
                }
            }
            if best.is_some() {
                return best;
            }
            // On to the next box, overlapping this one's core.
            t += CORE * voxel;
        }
        None
    }

    /// Is every chunk the drawn surface near `local` is read from meshed?
    fn meshed_near(&self, local: Vec3) -> bool {
        let s = self.surface.borrow();
        self.each_chunk_near(local, |c| s.chunks.contains_key(&c))
    }

    /// Mesh chunk `c` into the surface, unless it is there already.
    fn mesh_into(&self, c: [i32; 3], s: &mut Surface) {
        if s.chunks.contains_key(&c) {
            return;
        }
        let m = drawn_chunk(&self.field, c);
        s.chunks.insert(c, CollisionChunk::from_mesh(&m, self.bucket()));
        s.meshed_by_queries += 1;
    }

    /// The closest point on the drawn surface to `local`, and the surface's
    /// outward normal there, if one is in reach.
    fn closest_surface(&self, local: Vec3) -> Option<(Vec3, Vec3)> {
        if !self.mesh_accurate {
            return None;
        }
        self.closest_surface_at(local, self.field.d(local), false)
    }

    /// [`Self::closest_surface`], for a caller that has the field's distance
    /// `d` at `local` already and wants it again if no triangle answers.
    ///
    /// `distance_only`: the caller reads the distance to the answer and the
    /// side it is on, not which triangle gave it, so a bucket too far to
    /// hold a triangle the walk would test is not looked up at all. A point
    /// a metre or two above the ground, inside the band, walked a hundred
    /// buckets of two or four chunks each to find the ground under it, most
    /// of a fast craft's sweep. The walk tests the same triangles to the same
    /// nearest distance; only which of two triangles exactly as near is kept
    /// can differ, and both are that far.
    fn closest_surface_at(&self, local: Vec3, d: f32, distance_only: bool) -> Option<(Vec3, Vec3)> {
        if !self.mesh_accurate {
            return None;
        }
        // Cheap reject before any meshing: outside the band the field saturates
        // and there is nothing for a triangle to correct. Saturated, it reads
        // the band exactly, not more, so the band itself is the test: asking
        // for more than it let every query in open air run the whole search.
        if d.abs() >= self.field.band() {
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
        let reach = |k: i32| k - max_ring;
        let near = NearChunks::new(
            &s,
            (reach(c.0), reach(c.1), reach(c.2)),
            (c.0 + max_ring, c.1 + max_ring, c.2 + max_ring),
        );
        let each_in = |cell: (i32, i32, i32), f: &mut dyn FnMut(&SurfaceTri)| match &near {
            Some(near) => near.each_in(cell, f),
            None => s.each_in(cell, f),
        };
        // Buckets outside every nearby chunk's occupied range hold nothing,
        // so the walk is clamped to it: the same buckets in the same order,
        // less the empty ones. Without the table, nothing is clamped.
        let (olo, ohi) = near.as_ref().map_or(
            ((i32::MIN, i32::MIN, i32::MIN), (i32::MAX, i32::MAX, i32::MAX)),
            |n| n.occupied,
        );
        let tol = 1e-5 * (1.0 + local.abs().max_element());
        #[cfg(test)]
        let distance_only = distance_only && !TERRAIN_EVERY_BUCKET.with(|r| r.get());
        // A bound before the walk starts: the distance to some triangle near
        // the point, from the occupied bucket nearest it (under a point in
        // open air, the ground straight below). The walk is unchanged; a
        // triangle is skipped only when it cannot come within this bound,
        // and no triangle the walk would end on is ever that far. Whichever
        // ring the walk stops at, everything outside it is at least that
        // ring's floor away, and the walk only stops once its best is inside
        // the floor; so the answer is at most this bound, and every triangle
        // that could be the answer or tie with it is still tested, in the
        // same order.
        let mut seed = f32::INFINITY;
        if let Some(near) = &near
            && olo.0 <= ohi.0
        {
            let at = (c.0.clamp(olo.0, ohi.0), c.1.clamp(olo.1, ohi.1), c.2.clamp(olo.2, ohi.2));
            near.each_in(at, |tri| {
                let q = closest_point_on_triangle(local, tri.p[0], tri.p[1], tri.p[2]);
                let d2 = (q - local).length_squared();
                if d2.is_finite() {
                    seed = seed.min(d2);
                }
            });
        }
        for r in 0..=max_ring {
            for cz in (c.2 - r).max(olo.2)..=(c.2 + r).min(ohi.2) {
                for cy in (c.1 - r).max(olo.1)..=(c.1 + r).min(ohi.1) {
                    for cx in (c.0 - r).max(olo.0)..=(c.0 + r).min(ohi.0) {
                        // Only the shell: the inside was scanned at a smaller r.
                        let on_shell = (cx - c.0).abs() == r
                            || (cy - c.1).abs() == r
                            || (cz - c.2).abs() == r;
                        if !on_shell {
                            continue;
                        }
                        // No triangle in a bucket this far can pass the
                        // test below: each lies in every bucket its box
                        // touches, and the one holding its box's nearest
                        // point is at most that far, so it is tested there.
                        // Twice the slack, for the bucket's own rounding.
                        if distance_only && bucket_lower_d2(local, 2.0 * tol, (cx, cy, cz), cell) > best.min(seed) {
                            continue;
                        }
                        #[cfg(test)]
                        TERRAIN_BUCKETS.with(|n| n.set(n.get() + 1));
                        each_in((cx, cy, cz), &mut |tri| {
                            #[cfg(test)]
                            TERRAIN_VISITS.with(|n| n.set(n.get() + 1));
                            // Past the best so far, it can neither beat it nor
                            // tie it; see `box_lower_d2`.
                            if box_lower_d2(local, tol, &tri.p) > best.min(seed) {
                                return;
                            }
                            #[cfg(test)]
                            TERRAIN_TRIS.with(|n| n.set(n.get() + 1));
                            let q = closest_point_on_triangle(local, tri.p[0], tri.p[1], tri.p[2]);
                            let d2 = (q - local).length_squared();
                            if d2 < best {
                                best = d2;
                                hit = Some((q, normal_at(tri, q)));
                            }
                        });
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
            // Every bucket that holds anything has been walked: a wider ring
            // finds nothing.
            if (c.0 - r <= olo.0 && c.1 - r <= olo.1 && c.2 - r <= olo.2)
                && (c.0 + r >= ohi.0 && c.1 + r >= ohi.1 && c.2 + r >= ohi.2)
            {
                break;
            }
        }
        hit
    }
}

/// Rays into a chunk answered from a few of its cells before the chunk is
/// meshed whole for collision: a box of cells costs a few times what a ray at
/// a meshed chunk does, so a chunk asked about this often is worth meshing.
const RAYS_BEFORE_MESHING: u8 = 4;

/// The triangles of one chunk as drawn: stride 1, no skirt — the LOD-0
/// surface, which is what the renderer draws up close (it adds skirts to every
/// chunk it draws). A skirt is a curtain hung over the crack between two LODs;
/// as collision it would be an invisible wall around every chunk.
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
    /// The field first, then the drawn surface where the field says it is.
    ///
    /// The march every other shape gets steps at most a metre at a time and
    /// asks this collider its distance at every step, which near the ground
    /// means a closest-triangle search; a ray that misses walked its whole
    /// length that way, so a 124 m miss cost ten times a hit. The field's own
    /// trace strides through open air a band at a time and is cheap. The drawn
    /// surface is within about half a voxel of the field's, so a ray that never
    /// comes within a voxel of the field misses, and one that does is marched
    /// exactly from there for a few voxels. A ray that starts in rock, or whose
    /// march finds nothing in those few voxels, is handed to the general march.
    fn ray_cast(&self, o: Vec3, rd: Vec3, tmax: f32) -> RayAnswer {
        let scale = self.scale.max(1e-6);
        let local = self.to_local(o);
        if self.field.d(local) < 0.0 {
            return RayAnswer::March;
        }
        let ld = (self.rot.inverse() * rd).normalize_or_zero();
        // To within a voxel of the surface, not to the surface: the drawn
        // surface stands up to about half a voxel off the field's, so a ray
        // grazing a bump can pass over one and clip the other.
        let lvoxel = self.field.voxel();
        let Some(near) = self.field.raycast_within(local, ld, tmax / scale, lvoxel) else {
            return RayAnswer::Miss;
        };
        let voxel = lvoxel * scale;
        let field_t = near * scale;
        // Where the drawn surface has not been meshed for collision yet, the
        // few cells of it the ray reaches are meshed for this ray alone: the
        // same triangles, a few hundredths of the cost. Meshing the chunks
        // there, a chunk or eight a ray at full detail, made six script rays
        // cost 12.5 ms on a 1 km planet just after it was made, seventy times
        // what they cost once something had stood there.
        //
        // Only for a ray within about 37° of square to the ground: the march stops
        // within 0.02 of the surface, which a grazing ray reaches well before it
        // crosses, and the two answers must agree. A chunk a few rays have asked
        // about is meshed whole, as before, so a place asked about again and
        // again is cached: the first rays anywhere are cheap, and so are all
        // the ones after them.
        let at = local + ld * near;
        if self.mesh_accurate
            && !self.meshed_near(at)
            && self.field.grad(at).normalize_or_zero().dot(-ld) > 0.8
        {
            let c = (at / lvoxel).floor().to_array().map(|v| (v as i32).div_euclid(floptle_field::CHUNK));
            let asked = {
                let mut s = self.surface.borrow_mut();
                let n = s.asked.entry(c).or_insert(0);
                *n = n.saturating_add(1);
                *n
            };
            if asked <= RAYS_BEFORE_MESHING
                && let Some((t, n)) = self.hit_drawn_box(local, ld, near, tmax / scale)
            {
                return RayAnswer::Hit(t * scale, (self.rot * n).normalize_or_zero());
            }
        }
        let (mut t, end) = (field_t, (field_t + 6.0 * voxel).min(tmax));
        for _ in 0..512 {
            if t > end {
                break;
            }
            let p = o + rd * t;
            let d = self.distance(p);
            if d < 0.02 {
                return RayAnswer::Hit(t, self.ray_normal(p, rd));
            }
            t += d.clamp(0.02, 1.0);
        }
        RayAnswer::March
    }
    /// The drawn surface where its triangles are cached, and the field's own
    /// surface where they are not: a height straight from the voxels, within
    /// a fraction of a voxel of the drawn one, for about a hundredth of a
    /// millisecond. Never meshes anything. A ray asks a chunk about the fifth
    /// time it meshes the chunk whole, tens of milliseconds on the main thread,
    /// and settling a scatter field asked every new chunk hundreds of times;
    /// meshing even the few cells under one ray was ten times this.
    fn ground_cast(&self, o: Vec3, rd: Vec3, tmax: f32) -> RayAnswer {
        let scale = self.scale.max(1e-6);
        let local = self.to_local(o);
        // Starting in rock, there is no ground under it to stand on.
        if self.field.d(local) < 0.0 {
            return RayAnswer::Miss;
        }
        let ld = (self.rot.inverse() * rd).normalize_or_zero();
        let lvoxel = self.field.voxel();
        let Some(near) = self.field.raycast_within(local, ld, tmax / scale, lvoxel) else {
            return RayAnswer::Miss;
        };
        let at = local + ld * near;
        if self.mesh_accurate && self.meshed_near(at) {
            let voxel = lvoxel * scale;
            let (mut t, end) = (near * scale, (near * scale + 6.0 * voxel).min(tmax));
            while t <= end {
                let p = o + rd * t;
                // Every point the march asks has to be on triangles already
                // cached. The march runs a few voxels past where it started,
                // and a point whose reach crossed into a chunk not meshed yet
                // meshed that chunk here, whole, on a scatter prop's account:
                // tens of milliseconds while flying low over the edge of the
                // drawn ground. Past the cached ground, the field answers.
                if !self.meshed_near(self.to_local(p)) {
                    break;
                }
                let d = self.distance(p);
                if d < 0.02 {
                    return RayAnswer::Hit(t, self.ray_normal(p, rd));
                }
                t += d.clamp(0.02, 1.0);
            }
        }
        // The field's surface, traced the rest of the way.
        let mut t = near;
        for _ in 0..32 {
            let d = self.field.d(local + ld * t);
            if d.abs() < lvoxel * 1e-3 {
                break;
            }
            t += d;
        }
        if t * scale > tmax {
            return RayAnswer::Miss;
        }
        let n = self.rot * self.field.grad(local + ld * t).normalize_or_zero();
        RayAnswer::Hit(t * scale, if n.dot(rd) > 0.0 { -n } else { n })
    }
    fn distance(&self, p: Vec3) -> f32 {
        let local = self.to_local(p);
        let scale = self.scale.max(1e-6);
        // The field once: the band test and, in open air, the answer.
        let d = self.field.d(local);
        match self.closest_surface_at(local, d, true) {
            // Magnitude and side from the drawn surface: which way of the
            // closest point the point lies, judged by the surface's own
            // normal there. Exactly on the surface both are zero, and the
            // sign of a zero does not matter.
            Some((q, n)) => {
                let off = local - q;
                let side = if off.dot(n) < 0.0 { -1.0 } else { 1.0 };
                off.length() * side * scale
            }
            None => d * scale,
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
    /// Where the field reads open air at every point of the box, `distance`
    /// answers the field there, the band or more, and never asks a triangle:
    /// see [`floptle_field::ChunkField::open_air_in_box`]. So the box is
    /// clear of anything up to the band.
    fn clear_of(&self, lo: Vec3, hi: Vec3, r: f32) -> bool {
        let scale = self.scale.max(1e-6);
        #[allow(clippy::neg_cmp_op_on_partial_ord)]
        if !(r <= self.field.band() * scale) {
            return false;
        }
        // The box in the field's frame, grown by a rounding allowance: a
        // point inside the world box must land inside this one however
        // `to_local` rounds it.
        let m = floptle_core::math::Mat3::from_quat(self.rot.inverse());
        let (c, half) = ((lo + hi) * 0.5, (hi - lo) * 0.5);
        let half_l = (m.x_axis.abs() * half.x + m.y_axis.abs() * half.y + m.z_axis.abs() * half.z) / scale;
        let c_l = self.to_local(c);
        let pad = Vec3::splat(1e-4 * self.field.voxel() + 1e-5 * (1.0 + c_l.abs().max_element() + half_l.max_element()));
        self.field.open_air_in_box(c_l - half_l - pad, c_l + half_l + pad)
    }
    /// The drawn triangles under the box, with the drawn surface's normals:
    /// the same LOD-0 faces the collider meshes, from the same cache.
    fn triangles_in(&self, lo: Vec3, hi: Vec3, out: &mut Vec<FaceTri>) -> bool {
        let scale = self.scale.max(1e-6);
        let (llo, lhi) = self.local_box(lo, hi);
        let mut s = self.surface.borrow_mut();
        let cell = self.bucket();
        for ch in self.field.chunks_in_world_box(llo, lhi) {
            self.mesh_into(ch, &mut s);
        }
        let (a, b) = (cell_coord(llo, cell), cell_coord(lhi, cell));
        let mut seen: std::collections::HashSet<[u32; 9]> = std::collections::HashSet::new();
        for cz in a.2..=b.2 {
            for cy in a.1..=b.1 {
                for cx in a.0..=b.0 {
                    s.each_in((cx, cy, cz), |tri| {
                        if !tri_box_overlap(&tri.p, llo, lhi) {
                            return;
                        }
                        let key: [u32; 9] = std::array::from_fn(|i| tri.p[i / 3][i % 3].to_bits());
                        if !seen.insert(key) {
                            return;
                        }
                        out.push(FaceTri {
                            p: tri.p.map(|p| self.rot * (p * scale)),
                            n: tri.n.map(|n| self.rot * n),
                        });
                    });
                }
            }
        }
        true
    }
}

/// A squared distance from `p` that triangle `t` is certainly not closer than:
/// the distance to its box, pulled in by `tol` (a rounding allowance scaled to
/// the coordinates) and then by a thousandth, so it never lands above what
/// the closest-point test computes. Skipping a triangle whose bound is past
/// the best so far leaves the answer bit-identical, as in
/// [`TriMeshCollider::lower_d2`].
#[inline]
fn box_lower_d2(p: Vec3, tol: f32, t: &[Vec3; 3]) -> f32 {
    let (lo, hi) = (t[0].min(t[1]).min(t[2]), t[0].max(t[1]).max(t[2]));
    let d = ((lo - p).max(p - hi).max(Vec3::ZERO).length() - tol).max(0.0);
    d * d * 0.999
}

/// [`box_lower_d2`] for a bucket: below what every triangle bucketed in
/// grid cell `c` that touches it nearest there reads, `tol` of slack taken
/// off the distance.
fn bucket_lower_d2(p: Vec3, tol: f32, c: (i32, i32, i32), cell: f32) -> f32 {
    let d = (cell_min_dist2(p, c, cell).sqrt() - tol).max(0.0);
    d * d * 0.999
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

/// Where a ray from `o` along `rd` crosses triangle `abc`, as a distance
/// along the ray (Möller–Trumbore, both sides). `None` for a ray parallel to
/// the triangle's plane or one that passes outside it.
///
/// The edges are widened by a hair so that a ray aimed exactly at the seam
/// between two triangles is caught by one of them rather than slipping
/// between both.
fn ray_triangle(o: Vec3, rd: Vec3, a: Vec3, b: Vec3, c: Vec3) -> Option<f32> {
    const EDGE: f32 = 1e-4;
    let (e1, e2) = (b - a, c - a);
    let h = rd.cross(e2);
    let det = e1.dot(h);
    if det.abs() <= 1e-6 * e1.length() * e2.length() {
        return None;
    }
    let inv = 1.0 / det;
    let s = o - a;
    let u = s.dot(h) * inv;
    let q = s.cross(e1);
    let v = rd.dot(q) * inv;
    if u < -EDGE || v < -EDGE || u + v > 1.0 + EDGE {
        return None;
    }
    Some(e2.dot(q) * inv)
}

/// A static triangle-mesh collider — e.g. an imported map model you walk on. World-space
/// triangles are bucketed into a uniform spatial hash so closest-point queries only test
/// nearby triangles. Distance is unsigned (an imported map is rarely watertight); the body
/// is pushed out along `(p − closest)`, which for a surface you rest on points away from
/// the face. Resolved every substep, so a body never tunnels to the wrong side.
pub struct TriMeshCollider {
    tris: Vec<[Vec3; 3]>,
    /// Per triangle, parallel to `tris`: its unit normal and plane offset, and
    /// its box. Each gives a cheap lower bound on the distance to the triangle,
    /// which lets a query skip the closest-point test for a triangle that
    /// cannot beat what it already has. A floor is hundreds of coplanar
    /// triangles, and a point above it is the same height above every one.
    planes: Vec<(Vec3, f32)>,
    boxes: Vec<(Vec3, Vec3)>,
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
    /// Every vertex is inside this box. `distance` is to the nearest triangle,
    /// so it is never less than the distance to the box.
    aabb: (Vec3, Vec3),
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
    grid: CellGrid,
}

/// The mesh collider's triangle lists, one per cell.
///
/// A closest-point query looks at up to 125 cells, and a body on a mesh level
/// asks dozens of those a tick, so the lookup is the hot path. Flat when the
/// mesh's cell range is small enough to hold as an array — an index instead of
/// a hash — and a map otherwise (a long thin diagonal wall's box is mostly
/// empty cells). Both hand back the same lists in the same order, so which one
/// a mesh gets never changes an answer.
enum CellGrid {
    /// `start[i]..start[i + 1]` indexes `items` for cell `i` of the range
    /// `lo ..= lo + dims - 1`, x fastest.
    Flat { lo: (i32, i32, i32), dims: (usize, usize, usize), start: Vec<u32>, items: Vec<u32> },
    Sparse(std::collections::HashMap<(i32, i32, i32), Vec<u32>>),
}

impl CellGrid {
    /// Cells a mesh may spend on a flat grid: 4 bytes each.
    const FLAT_MAX: usize = 1 << 20;

    fn build(map: std::collections::HashMap<(i32, i32, i32), Vec<u32>>) -> Self {
        let Some(&first) = map.keys().next() else { return Self::Sparse(map) };
        let (lo, hi) = map.keys().fold((first, first), |(lo, hi), k| {
            ((lo.0.min(k.0), lo.1.min(k.1), lo.2.min(k.2)), (hi.0.max(k.0), hi.1.max(k.1), hi.2.max(k.2)))
        });
        let dims = (
            (hi.0 - lo.0 + 1) as usize,
            (hi.1 - lo.1 + 1) as usize,
            (hi.2 - lo.2 + 1) as usize,
        );
        let cells = dims.0.saturating_mul(dims.1).saturating_mul(dims.2);
        if cells > Self::FLAT_MAX || cells > (map.len() * 64).max(1 << 16) {
            return Self::Sparse(map);
        }
        let mut start = vec![0u32; cells + 1];
        let at = |k: &(i32, i32, i32)| {
            (k.0 - lo.0) as usize + dims.0 * ((k.1 - lo.1) as usize + dims.1 * (k.2 - lo.2) as usize)
        };
        for (k, v) in &map {
            start[at(k) + 1] = v.len() as u32;
        }
        for i in 1..start.len() {
            start[i] += start[i - 1];
        }
        let mut items = vec![0u32; start[cells] as usize];
        for (k, v) in &map {
            let i = at(k);
            items[start[i] as usize..start[i + 1] as usize].copy_from_slice(v);
        }
        Self::Flat { lo, dims, start, items }
    }

    /// The triangles registered in cell `c`, or `None` if it holds none.
    #[inline]
    fn get(&self, c: &(i32, i32, i32)) -> Option<&[u32]> {
        match self {
            Self::Flat { lo, dims, start, items } => {
                let (x, y, z) = (c.0 - lo.0, c.1 - lo.1, c.2 - lo.2);
                if x < 0 || y < 0 || z < 0 {
                    return None;
                }
                let (x, y, z) = (x as usize, y as usize, z as usize);
                if x >= dims.0 || y >= dims.1 || z >= dims.2 {
                    return None;
                }
                let i = x + dims.0 * (y + dims.1 * z);
                let (a, b) = (start[i] as usize, start[i + 1] as usize);
                (b > a).then(|| &items[a..b])
            }
            Self::Sparse(m) => m.get(c).map(Vec::as_slice),
        }
    }
}

impl TriMeshCollider {
    /// Spatial-hash cell size. The query searches `±SEARCH` cells, so a body up to
    /// `CELL·SEARCH` units in radius is guaranteed to find every triangle within its
    /// reach (5×5×5 block covers radii up to ~4 — far beyond any normal capsule).
    const CELL: f32 = 2.0;
    const SEARCH: i32 = 2;
    /// Closer to a face than this, `p - closest` is rounding, not a direction.
    const ON_SURFACE: f32 = 1e-3;
    /// How far from where a ray stopped a triangle can be and still be the
    /// one it met. The marches stop within 0.02 of a surface.
    const RAY_REACH: f32 = 0.025;

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
        let aabb = tris.iter().flatten().fold(
            (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY)),
            |(lo, hi), v| (lo.min(*v), hi.max(*v)),
        );
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
        let grid = CellGrid::build(grid);
        let planes = tris
            .iter()
            .map(|[a, b, c]| {
                let n = (*b - *a).cross(*c - *a).normalize();
                (n, n.dot(*a))
            })
            .collect();
        let boxes = tris.iter().map(|[a, b, c]| (a.min(*b).min(*c), a.max(*b).max(*c))).collect();
        Self { tris, planes, boxes, tri_label: kept_label, labels, cell, grid, bound, cells, aabb }
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
                        // Past the bound, it can be neither the closest nor
                        // tied with it; see `lower_d2`.
                        if self.lower_d2(p, ti) > bound {
                            continue;
                        }
                        #[cfg(test)]
                        MESH_TRIS.with(|n| n.set(n.get() + 1));
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
            if self.lower_d2(p, ti) > acc {
                return acc;
            }
            let t = self.tris[ti as usize];
            let d2 = (p - closest_point_on_triangle(p, t[0], t[1], t[2])).length_squared();
            if d2.is_finite() && d2 < acc { d2 } else { acc }
        })
    }

    /// A squared distance from `p` that triangle `ti` is certainly not closer
    /// than: the larger of its plane's and its box's. Skipping a triangle whose
    /// bound is already past the best leaves the answer bit-identical,
    /// triangle index included — as long as the bound never lands above the
    /// distance the full test would compute. Both are rounded, so the bound is
    /// pulled in by a tolerance scaled to the coordinates (a point exactly on
    /// a vertex is distance 0 from it, and its plane reads 1e-9) and then by a
    /// thousandth.
    #[inline]
    fn lower_d2(&self, p: Vec3, ti: u32) -> f32 {
        let (n, off) = self.planes[ti as usize];
        let tol = 1e-5 * (1.0 + p.abs().max_element() + off.abs());
        let plane = ((n.dot(p) - off).abs() - tol).max(0.0);
        let (lo, hi) = self.boxes[ti as usize];
        let out = ((lo - p).max(p - hi).max(Vec3::ZERO).length() - tol).max(0.0);
        let d = plane.max(out);
        d * d * 0.999
    }

    /// The squared distance [`Self::nearest_tri`] finds, where it is at most
    /// `limit2`; `None` where nothing is that close.
    ///
    /// The same walk over the same block, starting from `limit2` instead of
    /// from the closest triangle in `p`'s own cell. Every triangle the full
    /// search could pick that is within the limit is still tested, and the
    /// least of them is the full search's answer, so wherever this answers it
    /// answers the same bits. A sample metres from the mesh skips every cell
    /// and every triangle on the bound alone.
    fn nearest_d2_within(&self, p: Vec3, limit2: f32) -> Option<f32> {
        #[cfg(test)]
        MESH_BOUNDED.with(|n| n.set(n.get() + 1));
        let c = cell_coord(p, self.cell);
        let s = Self::SEARCH;
        let mut bound = limit2;
        let mut best: Option<f32> = None;
        let (lo, hi) = self.cells;
        for cz in (c.2 - s).max(lo.2)..=(c.2 + s).min(hi.2) {
            for cy in (c.1 - s).max(lo.1)..=(c.1 + s).min(hi.1) {
                for cx in (c.0 - s).max(lo.0)..=(c.0 + s).min(hi.0) {
                    if cell_min_dist2(p, (cx, cy, cz), self.cell) > bound {
                        continue;
                    }
                    let Some(list) = self.grid.get(&(cx, cy, cz)) else { continue };
                    for &ti in list {
                        if self.lower_d2(p, ti) > bound {
                            continue;
                        }
                        #[cfg(test)]
                        MESH_TRIS.with(|n| n.set(n.get() + 1));
                        let t = self.tris[ti as usize];
                        let q = closest_point_on_triangle(p, t[0], t[1], t[2]);
                        let d2 = (p - q).length_squared();
                        if d2.is_finite() && d2 <= bound && best.is_none_or(|bd| d2 < bd) {
                            best = Some(d2);
                            bound = d2;
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

    /// Triangle `ti`'s unit normal, by its winding. Never zero: the
    /// constructor keeps no zero-area triangle.
    fn face_normal(&self, ti: u32) -> Vec3 {
        let [a, b, c] = self.tris[ti as usize];
        (b - a).cross(c - a).normalize()
    }

    /// The triangle a ray along `rd` crosses first at or past `p`, among the
    /// ones within [`Self::RAY_REACH`] of it.
    ///
    /// Only nearby triangles, because the march already decided the ray
    /// stopped here; a face it would cross a metre on is not the one it hit.
    /// An exact tie, a ray down the seam of two triangles, goes to the lower
    /// index, so the answer does not depend on hash order.
    fn struck_tri(&self, p: Vec3, rd: Vec3) -> Option<u32> {
        let reach = Vec3::splat(Self::RAY_REACH);
        let (lo, hi) = (cell_coord(p - reach, self.cell), cell_coord(p + reach, self.cell));
        let mut best: Option<(f32, u32)> = None;
        for cz in lo.2..=hi.2 {
            for cy in lo.1..=hi.1 {
                for cx in lo.0..=hi.0 {
                    let Some(list) = self.grid.get(&(cx, cy, cz)) else { continue };
                    for &ti in list {
                        let [a, b, c] = self.tris[ti as usize];
                        let q = closest_point_on_triangle(p, a, b, c);
                        if (p - q).length_squared() > Self::RAY_REACH * Self::RAY_REACH {
                            continue;
                        }
                        // A hair behind `p`: a head-on march can stop a
                        // rounding error past the face it landed on.
                        let Some(t) = ray_triangle(p, rd, a, b, c) else { continue };
                        if t < -Self::ON_SURFACE {
                            continue;
                        }
                        if best.is_none_or(|(bt, bi)| t < bt || (t == bt && ti < bi)) {
                            best = Some((t, ti));
                        }
                    }
                }
            }
        }
        best.map(|(_, ti)| ti)
    }
}

impl CollisionShape for TriMeshCollider {
    /// The triangles in the grid cells the box covers, each once.
    fn triangles_in(&self, lo: Vec3, hi: Vec3, out: &mut Vec<FaceTri>) -> bool {
        if self.aabb.0.cmpgt(hi).any() || self.aabb.1.cmplt(lo).any() {
            return true;
        }
        let (a, b) = (cell_coord(lo, self.cell), cell_coord(hi, self.cell));
        let (clo, chi) = self.cells;
        let mut found: Vec<u32> = Vec::new();
        for cz in a.2.max(clo.2)..=b.2.min(chi.2) {
            for cy in a.1.max(clo.1)..=b.1.min(chi.1) {
                for cx in a.0.max(clo.0)..=b.0.min(chi.0) {
                    if let Some(list) = self.grid.get(&(cx, cy, cz)) {
                        found.extend_from_slice(list);
                    }
                }
            }
        }
        found.sort_unstable();
        found.dedup();
        for ti in found {
            let t = self.tris[ti as usize];
            if tri_box_overlap(&t, lo, hi) {
                out.push(FaceTri::flat(t));
            }
        }
        true
    }
    /// The first triangle the ray crosses, found by walking the cells of the
    /// mesh's own grid along it (3D DDA) rather than marching a distance
    /// field: a ray costs the triangles in the cells it passes through, not a
    /// closest-point search per step.
    fn ray_cast(&self, o: Vec3, rd: Vec3, tmax: f32) -> RayAnswer {
        let Some((t0, t1)) = ray_box_span(o, rd, self.aabb.0, self.aabb.1, tmax) else {
            return RayAnswer::Miss;
        };
        let cell = self.cell;
        let start = o + rd * t0;
        let (mut c, lo, hi) = (cell_coord(start, cell), self.cells.0, self.cells.1);
        let clamp = |v: i32, a: i32, b: i32| v.max(a).min(b);
        c = (clamp(c.0, lo.0, hi.0), clamp(c.1, lo.1, hi.1), clamp(c.2, lo.2, hi.2));
        let step = |d: f32| if d > 0.0 { 1 } else if d < 0.0 { -1 } else { 0 };
        let st = (step(rd.x), step(rd.y), step(rd.z));
        // t at which the ray crosses the next cell wall on each axis.
        let next = |ci: i32, s: i32, oi: f32, di: f32| -> (f32, f32) {
            if s == 0 {
                return (f32::INFINITY, f32::INFINITY);
            }
            let wall = if s > 0 { (ci + 1) as f32 * cell } else { ci as f32 * cell };
            ((wall - oi) / di, cell / di.abs())
        };
        let (mut tx, dx) = next(c.0, st.0, o.x, rd.x);
        let (mut ty, dy) = next(c.1, st.1, o.y, rd.y);
        let (mut tz, dz) = next(c.2, st.2, o.z, rd.z);
        let mut best: Option<(f32, u32)> = None;
        loop {
            if let Some(list) = self.grid.get(&c) {
                for &ti in list {
                    let [a, b, cc] = self.tris[ti as usize];
                    let Some(t) = ray_triangle(o, rd, a, b, cc) else { continue };
                    if t < 0.0 || t > tmax {
                        continue;
                    }
                    if best.is_none_or(|(bt, bi)| t < bt || (t == bt && ti < bi)) {
                        best = Some((t, ti));
                    }
                }
            }
            let exit = tx.min(ty).min(tz);
            if best.is_some_and(|(bt, _)| bt <= exit) || exit > t1 {
                break;
            }
            if tx <= ty && tx <= tz {
                c.0 += st.0;
                tx += dx;
            } else if ty <= tz {
                c.1 += st.1;
                ty += dy;
            } else {
                c.2 += st.2;
                tz += dz;
            }
            if c.0 < lo.0 || c.0 > hi.0 || c.1 < lo.1 || c.1 > hi.1 || c.2 < lo.2 || c.2 > hi.2 {
                break;
            }
        }
        match best {
            Some((t, ti)) => {
                let n = self.face_normal(ti);
                RayAnswer::Hit(t, if n.dot(rd) > 0.0 { -n } else { n })
            }
            None => RayAnswer::Miss,
        }
    }
    fn bounds(&self) -> Option<(Vec3, f32)> {
        Some(self.bound)
    }
    fn aabb(&self) -> Option<(Vec3, Vec3)> {
        // An empty mesh's box is inverted (+inf..-inf), which every gap test
        // reads as infinitely far: correct, since it has nothing to touch.
        Some(self.aabb)
    }
    fn distance(&self, p: Vec3) -> f32 {
        // No nearby triangle → far away (no collision). Unsigned, so always ≥ 0.
        self.nearest(p).map(|(_, d2)| d2.sqrt()).unwrap_or(1e6)
    }
    /// Unsigned, so nothing is under a limit of zero or less, and a box
    /// corner (a sample of no radius) never needs a search at all. Otherwise
    /// the search starts bounded by `r`: a hair over `r²`, so a distance that
    /// rounds to just under `r` is never lost to the squaring.
    fn distance_within(&self, p: Vec3, r: f32) -> f32 {
        #[allow(clippy::neg_cmp_op_on_partial_ord)]
        if !(r > 0.0) {
            return 1e6;
        }
        let (lo, hi) = self.aabb;
        let gap = (lo - p).max(p - hi).max(Vec3::ZERO);
        if gap.length_squared() > r * r * 1.001 + 1e-6 {
            return 1e6f32.max(r);
        }
        let limit2 = if r.is_finite() { r * r * 1.001 + 1e-9 } else { f32::INFINITY };
        self.nearest_d2_within(p, limit2).map(|d2| d2.sqrt()).unwrap_or(1e6f32.max(r))
    }
    /// Away from the nearest point on the mesh, which rounds a body over an
    /// edge rather than catching it on one.
    ///
    /// On the surface itself there is no away: `p - closest` is rounding. The
    /// face's own axis is still exact there, and which side of it is the
    /// outside is not knowable from a point on the face, so it answers the
    /// side facing up, the guess the old `+Y` fallback made. A caller that
    /// knows where it came from asks [`CollisionShape::ray_normal`] instead.
    fn normal(&self, p: Vec3) -> Vec3 {
        match self.nearest_tri(p) {
            Some((q, d2, _)) if d2 > Self::ON_SURFACE * Self::ON_SURFACE => (p - q).normalize(),
            Some((_, _, ti)) => {
                let n = self.face_normal(ti);
                if n.y < 0.0 { -n } else { n }
            }
            None => Vec3::Y,
        }
    }
    /// `None` on the surface, where [`Self::normal`]'s side is a guess, so a
    /// contact there falls back to the body's own motion.
    fn normal_reliable(&self, p: Vec3) -> Option<Vec3> {
        let (q, d2, _) = self.nearest_tri(p)?;
        (d2 > Self::ON_SURFACE * Self::ON_SURFACE).then(|| (p - q).normalize())
    }
    /// The face the ray crossed, facing back along it.
    ///
    /// A ray that stopped short of the mesh without crossing a face within
    /// reach grazed a silhouette edge; that one answers from its closest
    /// point, which is off the surface and so a real direction.
    fn ray_normal(&self, p: Vec3, rd: Vec3) -> Vec3 {
        let n = match self.struck_tri(p, rd) {
            Some(ti) => self.face_normal(ti),
            None => match self.nearest_tri(p) {
                Some((q, d2, _)) if d2 > Self::ON_SURFACE * Self::ON_SURFACE => {
                    return (p - q).normalize();
                }
                Some((_, _, ti)) => self.face_normal(ti),
                None => return -rd,
            },
        };
        if n.dot(rd) > 0.0 { -n } else { n }
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

    /// A face spanning many cells of the grid comes back once, and a box
    /// nowhere near the mesh gets nothing.
    #[test]
    fn a_mesh_hands_out_each_face_in_a_box_once() {
        let verts = [
            Vec3::new(-20.0, 0.0, -20.0),
            Vec3::new(20.0, 0.0, -20.0),
            Vec3::new(0.0, 0.0, 20.0),
            Vec3::new(100.0, 0.0, 100.0),
            Vec3::new(101.0, 0.0, 100.0),
            Vec3::new(100.0, 0.0, 101.0),
        ];
        let m = TriMeshCollider::new(&verts, &[0, 2, 1, 3, 5, 4]);
        let mut out = Vec::new();
        assert!(m.triangles_in(Vec3::new(-6.0, -1.0, -6.0), Vec3::new(6.0, 1.0, 6.0), &mut out));
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].p, [verts[0], verts[2], verts[1]]);
        assert!((out[0].n[0] - Vec3::Y).length() < 1e-5, "wound up, faces up: {:?}", out[0].n);
        out.clear();
        m.triangles_in(Vec3::new(50.0, -1.0, 50.0), Vec3::new(51.0, 1.0, 51.0), &mut out);
        assert!(out.is_empty(), "{out:?}");
    }

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

    /// The closest-point walk with none of its skips: every cell of the block,
    /// every triangle in it, first strictly-closer wins.
    fn nearest_reference(m: &TriMeshCollider, p: Vec3) -> Option<(Vec3, f32, u32)> {
        let c = cell_coord(p, m.cell);
        let s = TriMeshCollider::SEARCH;
        let mut best: Option<(Vec3, f32, u32)> = None;
        for cz in c.2 - s..=c.2 + s {
            for cy in c.1 - s..=c.1 + s {
                for cx in c.0 - s..=c.0 + s {
                    let Some(list) = m.grid.get(&(cx, cy, cz)) else { continue };
                    for &ti in list {
                        let t = m.tris[ti as usize];
                        let q = closest_point_on_triangle(p, t[0], t[1], t[2]);
                        let d2 = (p - q).length_squared();
                        if d2.is_finite() && best.is_none_or(|(_, bd, _)| d2 < bd) {
                            best = Some((q, d2, ti));
                        }
                    }
                }
            }
        }
        best
    }

    /// The walk as it was before triangles could be skipped: the cell skips
    /// and the seeded bound, nothing else.
    fn nearest_cells_only(m: &TriMeshCollider, p: Vec3) -> Option<(Vec3, f32, u32)> {
        let c = cell_coord(p, m.cell);
        let s = TriMeshCollider::SEARCH;
        let mut best: Option<(Vec3, f32, u32)> = None;
        let mut bound = m.grid.get(&c).map_or(f32::INFINITY, |list| {
            list.iter().fold(f32::INFINITY, |acc, &ti| {
                let t = m.tris[ti as usize];
                let d2 = (p - closest_point_on_triangle(p, t[0], t[1], t[2])).length_squared();
                if d2.is_finite() && d2 < acc { d2 } else { acc }
            })
        });
        let (lo, hi) = m.cells;
        for cz in (c.2 - s).max(lo.2)..=(c.2 + s).min(hi.2) {
            for cy in (c.1 - s).max(lo.1)..=(c.1 + s).min(hi.1) {
                for cx in (c.0 - s).max(lo.0)..=(c.0 + s).min(hi.0) {
                    if cell_min_dist2(p, (cx, cy, cz), m.cell) > bound {
                        continue;
                    }
                    let Some(list) = m.grid.get(&(cx, cy, cz)) else { continue };
                    for &ti in list {
                        let t = m.tris[ti as usize];
                        let q = closest_point_on_triangle(p, t[0], t[1], t[2]);
                        let d2 = (p - q).length_squared();
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

    /// **Every skip in the mesh query is exact.** Physics resimulates rollback
    /// ticks and needs the same answer, bit for bit, whichever cells and
    /// triangles the query was able to rule out. Checked over a subdivided
    /// floor (hundreds of coplanar triangles: every one the same height below
    /// a point), a wall, and points on the surface, on shared edges and in the
    /// air:
    ///
    /// * the point and distance equal a walk with no skips at all;
    /// * point, distance and triangle equal the walk before triangles could be
    ///   skipped. Which of two triangles meeting at a vertex a point is
    ///   nearest to is an exact tie, and the cell skips already broke those
    ///   differently from the unskipped walk (deterministically, so rollback
    ///   never minded); skipping triangles must not break them differently
    ///   again.
    #[test]
    fn the_pruned_mesh_query_answers_exactly_what_the_full_walk_does() {
        let mut verts = Vec::new();
        let mut idx = Vec::new();
        // A 24 m floor of 0.75 m quads, gently warped so it is not all one plane.
        let n = 32u32;
        for z in 0..=n {
            for x in 0..=n {
                let (fx, fz) = (x as f32 * 0.75 - 12.0, z as f32 * 0.75 - 12.0);
                verts.push(Vec3::new(fx, 0.02 * (fx * 0.7).sin() * (fz * 0.3).cos(), fz));
            }
        }
        for z in 0..n {
            for x in 0..n {
                let a = z * (n + 1) + x;
                idx.extend([a, a + 1, a + n + 1, a + 1, a + n + 2, a + n + 1]);
            }
        }
        // A wall standing on it.
        let w = verts.len() as u32;
        verts.extend([
            Vec3::new(-3.0, 0.0, 2.0),
            Vec3::new(3.0, 0.0, 2.0),
            Vec3::new(3.0, 4.0, 2.0),
            Vec3::new(-3.0, 4.0, 2.0),
        ]);
        idx.extend([w, w + 1, w + 2, w, w + 2, w + 3]);
        let m = TriMeshCollider::new(&verts, &idx);

        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 40) as f32 / (1u64 << 24) as f32
        };
        let mut probes: Vec<Vec3> = (0..6000)
            .map(|_| Vec3::new(rnd() * 30.0 - 15.0, rnd() * 7.0 - 1.5, rnd() * 30.0 - 15.0))
            .collect();
        // Exactly on vertices and edge midpoints: the ties.
        for t in m.tris.iter().step_by(37) {
            probes.extend([t[0], (t[0] + t[1]) * 0.5, (t[1] + t[2]) * 0.5 + Vec3::Y * 0.4]);
        }
        let mut hits = 0;
        for p in probes {
            let got = m.nearest_tri(p);
            let bits = |r: Option<(Vec3, f32, u32)>| r.map(|(q, d2, ti)| (q.to_array().map(f32::to_bits), d2.to_bits(), ti));
            assert_eq!(bits(got), bits(nearest_cells_only(&m, p)), "at {p}: differs from the cell-skip walk");
            let full = nearest_reference(&m, p);
            assert_eq!(
                bits(got).map(|(q, d2, _)| (q, d2)),
                bits(full).map(|(q, d2, _)| (q, d2)),
                "at {p}: a different point than the full walk"
            );
            hits += usize::from(got.is_some());
        }
        assert!(hits > 3000, "the probes mostly missed the mesh ({hits}), so they proved little");
    }
}

#[cfg(test)]
mod drawn_surface_tests {
    use super::*;
    use floptle_field::{Brush, BrushProfile, ChunkField};

    /// A posed terrain hands out the triangles it collides with, each once,
    /// in the anchor frame, with the drawn surface's normals turned with it.
    #[test]
    fn terrain_hands_out_its_drawn_faces_with_their_normals() {
        let rot = Quat::from_rotation_z(0.3);
        let t = ChunkTerrain::posed(hill(), rot, 2.0);
        let mut out = Vec::new();
        let inside = t.distance(Vec3::ZERO);
        assert!(t.triangles_in(Vec3::splat(-30.0), Vec3::splat(30.0), &mut out));
        assert!(out.len() > 20, "only {} faces; the box centre is {inside} from the surface", out.len());
        let mut seen = std::collections::HashSet::new();
        for f in &out {
            let key: [u32; 9] = std::array::from_fn(|i| f.p[i / 3][i % 3].to_bits());
            assert!(seen.insert(key), "a face came back twice");
            let c = (f.p[0] + f.p[1] + f.p[2]) / 3.0;
            assert!(t.distance(c).abs() < 1e-3, "a face centre is {} off the collider", t.distance(c));
            let face = (f.p[1] - f.p[0]).cross(f.p[2] - f.p[0]).normalize();
            for n in f.n {
                assert!(n.dot(face) > 0.3, "normal {n} does not face with {face}");
            }
        }
    }

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

    /// **Colliding with the drawn surface is not dear.** Twenty bodies
    /// resting on rough ground, the state a game is in almost all of the time
    /// and the one the query is asked hardest, and how many triangles the
    /// queries walk a step: every one a bucket hands over, whether its box
    /// lets it be skipped or not.
    ///
    /// Counted, not timed. It was a ratio of two timings, the drawn surface
    /// against the field collider, at about 4x with a ceiling of 7; once the
    /// field's own read got cheaper the ratio sat near 5 and read 8.8 on a
    /// loaded machine with nothing wrong. Triangle buckets sized to the query
    /// reach rather than to a voxel put every triangle in the band into the
    /// bucket a resting body stands in, and finding the one under its feet
    /// walked hundreds of them; so does a ring search that stops stopping
    /// early. Measured at about 4,700 a step; buckets one reach across read
    /// 34,000 and a ring search that never stops early 58,000.
    #[test]
    fn colliding_with_the_drawn_surface_is_not_dear() {
        let mut t = ChunkTerrain::new(rough());
        t.mesh_accurate = true;
        let mut w = crate::PhysicsWorld::new(Default::default());
        w.add_collider(Box::new(t));
        for i in 0..20 {
            let a = i as f32 * 0.31;
            w.add_body(crate::Body::sphere(Vec3::new(a.cos() * 4.0, 6.0, a.sin() * 4.0), 0.4));
        }
        for _ in 0..120 {
            w.step(1.0 / 60.0); // let them land — settle is not the subject
        }
        let resting = w.bodies.iter().filter(|b| b.asleep || b.vel.length() < 0.5).count();
        TERRAIN_VISITS.with(|n| n.set(0));
        let steps = 20;
        for _ in 0..steps {
            w.step(1.0 / 60.0);
        }
        let walked = TERRAIN_VISITS.with(|n| n.get()) / steps;
        eprintln!("triangles walked a step for 20 resting bodies: {walked}");
        assert!(resting >= 15, "only {resting} of 20 bodies came to rest: the setup is not the resting case");
        assert!(
            walked <= 10_000,
            "the drawn-surface queries walked {walked} triangles a step for twenty resting bodies. \
             Past this something has stopped narrowing — check that the triangle buckets are \
             still one VOXEL and not one reach, and that the expanding-ring search still stops early."
        );
    }

    /// A point above the ground, inside the band where the drawn surface
    /// answers, finds the ground under it without looking up the buckets too
    /// far away to hold anything nearer: under half of them a metre and
    /// more up, where a craft's sweep asks most (97,044 of 212,881 here). Its distance is the one
    /// the full walk finds, bit for bit, over rough ground, across a chunk's
    /// faces and its corner, from inside the rock to the edge of the band.
    #[test]
    fn a_distance_above_the_ground_skips_the_buckets_too_far_to_matter() {
        let mut t = ChunkTerrain::new(rough());
        t.mesh_accurate = true;
        let mut points = Vec::new();
        for i in 0..28 {
            for k in 0..28 {
                for j in 0..18 {
                    points.push(Vec3::new(i as f32 * 0.37 - 5.0, j as f32 * 0.29 - 1.0, k as f32 * 0.37 - 5.0));
                }
            }
        }
        let walk = |every: bool, high: bool| {
            TERRAIN_EVERY_BUCKET.with(|r| r.set(every));
            TERRAIN_BUCKETS.with(|n| n.set(0));
            let d: Vec<u32> = points
                .iter()
                .filter(|p| !high || {
                    let d = t.field.d(**p);
                    d > 1.0 && d < t.field.band()
                })
                .map(|p| t.distance(*p).to_bits())
                .collect();
            TERRAIN_EVERY_BUCKET.with(|r| r.set(false));
            (d, TERRAIN_BUCKETS.with(|n| n.get()))
        };
        let (every, _) = walk(true, false);
        let (skipping, _) = walk(false, false);
        assert_eq!(every.len(), points.len());
        let differs = (0..every.len()).find(|&k| every[k] != skipping[k]);
        assert_eq!(differs.map(|k| points[k]), None, "skipping buckets, a distance came out different");
        let (high, all) = walk(true, true);
        let (_, few) = walk(false, true);
        assert!(high.len() > 400, "fixture: only {} points a metre or more up inside the band", high.len());
        assert!(few * 2 <= all, "a metre or more up, the walk looked up {few} buckets of the {all} it used to");
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
        // surface visibly drops away from it, and gently enough that the probe
        // stays inside the band: past it the field answers on its own and the
        // cache is never asked.
        for _ in 0..3 {
            t.field.sculpt(
                Brush::Lower,
                probe - Vec3::Y * 0.5,
                3.0,
                0.2,
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

    /// **A dig forgets only the chunks it could have changed.** Dropping the
    /// whole cache made the next query near every body re-mesh every chunk it
    /// stood in, a few milliseconds of physics on each bite of a mining laser.
    /// Two hills four chunks apart: a dig on one leaves the other's triangles
    /// cached, and the dug ground still reads open.
    #[test]
    fn a_dig_forgets_only_the_chunks_it_reached() {
        let mut f = ChunkField::new(0.5);
        let (a, b) = (Vec3::new(0.0, 0.0, 0.0), Vec3::new(64.0, 0.0, 0.0));
        for _ in 0..30 {
            f.sculpt(Brush::Raise, a, 6.0, 1.0, BrushProfile::default());
            f.sculpt(Brush::Raise, b, 6.0, 1.0, BrushProfile::default());
        }
        let mut t = ChunkTerrain::new(f);
        let crown = |t: &ChunkTerrain, at: Vec3| {
            let top = (0..400)
                .map(|i| 8.0 - i as f32 * 0.05)
                .find(|y| t.field.d(at + Vec3::Y * *y) <= 0.0)
                .expect("the hill has a surface");
            at + Vec3::Y * (top + t.field.voxel())
        };
        let (pa, pb) = (crown(&t, a), crown(&t, b));
        let before = t.distance(pa);
        t.distance(pb);
        let far: Vec<[i32; 3]> = t.surface.borrow().chunks.keys().copied().filter(|c| c[0] >= 3).collect();
        assert!(!far.is_empty(), "the far hill was never meshed");
        let mut written = Vec::new();
        for _ in 0..40 {
            written.extend(t.field.sculpt(Brush::Lower, pa - Vec3::Y * 0.5, 3.0, 1.0, BrushProfile::default()));
        }
        written.sort_unstable();
        written.dedup();
        t.invalidate_chunks(&written);
        let s = t.surface.borrow();
        assert!(far.iter().all(|c| s.chunks.contains_key(c)), "the dig forgot the far hill's chunks too");
        assert!(written.iter().all(|c| !s.chunks.contains_key(c)), "a chunk the dig wrote is still cached");
        drop(s);
        let fresh = t.distance(pa);
        assert!(fresh > before + 0.1, "the dug ground still reads as it was: {before} -> {fresh}");
    }

    /// **A dig re-meshes no collision chunk on the main thread.** Fifty bites of
    /// a mining laser into a generated planet, each where the player stands. The
    /// collider copies the voxels the bite wrote and keeps its old triangles,
    /// which a query reads without meshing anything; the chunks whose meshes the
    /// bite reached are meshed elsewhere and adopted, and then the query sees
    /// the hole. Forgetting every touched chunk and its neighbours made the next
    /// query re-mesh a few dozen of them on the main thread, tens of milliseconds
    /// a bite. What is left on it is the copy, the gathers and the bucketing.
    /// **Dropping props onto new ground meshes nothing.** Six hundred settle
    /// rays straight down onto a patch of planet nobody has stood on. The
    /// ordinary ray meshes each chunk whole once a few rays have asked it,
    /// tens of milliseconds on the main thread apiece: every spike of a
    /// scatter field arriving under a walking or flying player. The ground
    /// query reads the height from the field there, meshes no chunk, lands
    /// within a fraction of a voxel of the drawn triangles, and costs a small
    /// part of the ordinary rays' time. Totals, not single rays, so a busy
    /// machine pausing one ray cannot decide it.
    #[test]
    fn a_ground_query_meets_the_drawn_surface_without_meshing_it() {
        let radius = 150.0;
        let voxel = 1.5;
        let field = floptle_field::procgen::generate_planet(&floptle_field::procgen::PlanetFill {
            seed: 11,
            radius,
            voxel,
            relief: radius * 0.065,
            cave_depth: radius * 0.35,
            core_r: 10.5,
            ..Default::default()
        });
        let ray = ChunkTerrain::new(field.clone());
        let ground = ChunkTerrain::new(field);
        let (mut ray_s, mut ground_s, mut far, mut n) = (0f64, 0f64, 0f32, 0);
        for k in 0..600 {
            // A patch about 40 m across on one side of the planet.
            let (a, b) = ((k % 25) as f32 * 0.011, (k / 25) as f32 * 0.011);
            let up = Vec3::new(a.sin(), b.sin(), 1.0).normalize();
            let from = up * (radius + 60.0);
            let t = std::time::Instant::now();
            let r = ray.ray_cast(from, -up, 150.0);
            ray_s += t.elapsed().as_secs_f64();
            let t = std::time::Instant::now();
            let g = ground.ground_cast(from, -up, 150.0);
            ground_s += t.elapsed().as_secs_f64();
            if let (RayAnswer::Hit(tr, _), RayAnswer::Hit(tg, _)) = (r, g) {
                far = far.max((tr - tg).abs());
                n += 1;
            }
        }
        assert!(n > 500, "only {n} of 600 rays hit both ways");
        assert!(!ray.surface.borrow().chunks.is_empty(), "the ordinary ray meshed nothing; the comparison means nothing");
        assert_eq!(ground.surface.borrow().chunks.len(), 0, "the ground query meshed a chunk");
        assert!(far < voxel * 0.25, "the ground query is up to {far} m off the drawn surface");
        assert!(
            ground_s * 4.0 < ray_s,
            "600 ground queries took {:.2} ms against the ordinary rays' {:.2} ms",
            ground_s * 1e3,
            ray_s * 1e3
        );
    }

    #[test]
    fn a_dig_re_meshes_no_collision_chunk_on_the_main_thread() {
        use floptle_field::{Brush, BrushProfile};
        let radius = 150.0;
        let mut field = floptle_field::procgen::generate_planet(&floptle_field::procgen::PlanetFill {
            seed: 11,
            radius,
            voxel: 1.5,
            relief: radius * 0.065,
            cave_depth: radius * 0.35,
            core_r: 10.5,
            ..Default::default()
        });
        let _ = field.take_changed();
        let mut t = ChunkTerrain::new(field.clone());
        let (mut stale_n, mut main_ms, mut moved) = (Vec::new(), Vec::new(), 0);
        for k in 0..50 {
            let a = k as f32 * 0.071;
            let dir = Vec3::new(a.cos(), 0.35, a.sin()).normalize();
            let Some(ground) = field.raycast(dir * radius * 1.5, -dir, radius) else { continue };
            let feet = ground + dir * 0.9;
            let before = t.distance(feet);
            let meshed_before = t.surface.borrow().chunks.len();
            let touched = field.sculpt(Brush::Lower, ground, 2.4, 0.85, BrushProfile::default());
            let start = std::time::Instant::now();
            let (lo, hi) = field.take_changed().expect("the bite wrote voxels");
            let mut copy = field.chunks_holding(lo, hi);
            copy.extend_from_slice(&touched);
            t.field.copy_chunks_from(&field, &copy);
            let stale: Vec<[i32; 3]> = field.chunks_reading(lo, hi, 1).into_iter().filter(|c| t.is_meshed(*c)).collect();
            let scratches: Vec<_> = stale.iter().map(|c| floptle_field::scratch_for_chunk(&field, *c, 1)).collect();
            let mut main = start.elapsed();
            // Until the new meshes land the old triangles answer, and nothing meshes.
            assert_eq!(t.distance(feet), before, "the old triangles must answer until the new ones land");
            assert_eq!(t.surface.borrow().chunks.len(), meshed_before, "a query meshed a chunk on the main thread");
            // The worker's half: meshing and bucketing.
            let built: Vec<_> = scratches
                .iter()
                .map(|sc| CollisionChunk::from_mesh(&floptle_field::mesh_scratch(sc, false), field.voxel()))
                .collect();
            let adopt = std::time::Instant::now();
            for (c, b) in stale.iter().zip(built) {
                t.adopt(*c, b);
            }
            main += adopt.elapsed();
            if t.distance(feet) > before + 0.05 {
                moved += 1;
            }
            stale_n.push(stale.len());
            main_ms.push(main.as_secs_f64() * 1e3);
        }
        assert!(stale_n.len() >= 40, "only {} bites found the ground", stale_n.len());
        assert!(moved * 10 >= stale_n.len() * 9, "only {moved} of {} bites showed in the collider once its meshes landed", stale_n.len());
        main_ms.sort_by(f64::total_cmp);
        stale_n.sort_unstable();
        println!(
            "{} bites: {} collision chunks re-meshed off the main thread (median); main thread p50 {:.2} ms, p90 {:.2} ms",
            stale_n.len(),
            stale_n[stale_n.len() / 2],
            main_ms[main_ms.len() / 2],
            main_ms[main_ms.len() * 9 / 10]
        );
    }

    /// **A short ray at the ground finds what the march finds, for no more.**
    /// A player's tool aim, a camera's roof probe and a walker each cast a few
    /// metres from just above the ground. A thousand 5 m rays over a generated
    /// planet, from 2 m up and in every direction down and across, take about
    /// 0.03 ms each on a desktop. Tracing the field to its surface, rather than
    /// to within a voxel of it, missed three of them: rays grazing a bump that
    /// passed over the field's surface and clipped the drawn one.
    ///
    /// The march was dearer than this path until a drawn-surface query got
    /// cheap; now the two cost about the same for a ray this short. Meshing
    /// the cells under every ray, which this path exists to avoid, costs
    /// several times the march, and is what this counts.
    #[test]
    fn a_short_ray_at_the_ground_finds_what_the_march_finds() {
        let radius = 150.0;
        let field = floptle_field::procgen::generate_planet(&floptle_field::procgen::PlanetFill {
            seed: 11,
            radius,
            voxel: 1.5,
            relief: radius * 0.065,
            cave_depth: radius * 0.35,
            core_r: 10.5,
            ..Default::default()
        });
        let ground = ChunkTerrain::new(field.clone());
        let cols = vec![crate::AnchoredCollider::world(Box::new(ChunkTerrain::new(field.clone())))];
        let slow = vec![crate::AnchoredCollider::world(Box::new(MarchOnly(ChunkTerrain::new(field))))];
        let mut r = floptle_core::noise::Rng::new(9);
        let mut rays = Vec::new();
        while rays.len() < 1000 {
            let d = Vec3::new(r.range(-1.0, 1.0) as f32, r.range(-1.0, 1.0) as f32, r.range(-1.0, 1.0) as f32).normalize_or_zero();
            if d == Vec3::ZERO {
                continue;
            }
            // Down onto the surface from the outside, then 2 m back up.
            let Some(hit) = ground.field.raycast(d * (radius * 1.5), -d, radius) else { continue };
            let o = hit + d * 2.0;
            let side = d.cross(if d.y.abs() < 0.9 { Vec3::Y } else { Vec3::X }).normalize();
            let k = r.range(0.0, 1.0) as f32;
            rays.push((o, (-d * k + side * (1.0 - k)).normalize()));
        }
        let cast = |cols: &[crate::AnchoredCollider]| {
            let t = std::time::Instant::now();
            let hits: Vec<Option<f32>> =
                rays.iter().map(|(o, d)| crate::raycast_colliders(cols, *o, *d, 5.0, !0).map(|h| h.distance)).collect();
            (hits, t.elapsed().as_secs_f64() * 1e3 / rays.len() as f64)
        };
        cast(&cols);
        cast(&slow);
        let (hits, ms) = cast(&cols);
        let (march, march_ms) = cast(&slow);
        let n = hits.iter().filter(|h| h.is_some()).count();
        println!("1000 rays of 5 m from 2 m up: {ms:.4} ms each ({n} hit); the march {march_ms:.4}");
        assert!(n > 300, "the rays should mostly reach the ground: {n}");
        for (i, (a, b)) in hits.iter().zip(&march).enumerate() {
            match (a, b) {
                (Some(a), Some(b)) => assert!((a - b).abs() < 0.05, "ray {i}: {a} against the march's {b}"),
                _ => assert_eq!(a.is_some(), b.is_some(), "ray {i}: {a:?} against the march's {b:?}"),
            }
        }
        // Counted, not timed: the two paths cost about the same for a ray this
        // short, so a ratio of their times read 1.38 on a CI runner with
        // nothing wrong. What this path must not do is mesh the cells under
        // every ray: the thousand rays mesh 222 chunks between them, each once.
        let meshed = cols[0].shape.chunk_terrain().unwrap().meshed_by_queries();
        assert!(meshed <= 300, "1000 short rays meshed {meshed} chunks ({ms:.4} ms each, the march {march_ms:.4})");
    }

    /// **A ray at a planet nothing has stood on costs about what one at
    /// ground already meshed for collision does.** A ray meshed the collision
    /// chunks where it landed, a chunk or eight at full detail, so six script
    /// rays at a 1 km planet just made cost 12.5 ms, against 0.18 ms once the
    /// ground there was meshed. Rays straight down at 60 places, each on a
    /// fresh collider and on one whose ground is meshed already; the answers
    /// agree.
    #[test]
    fn a_ray_at_ground_never_stood_on_costs_about_what_one_at_meshed_ground_does() {
        let radius = 150.0;
        let field = floptle_field::procgen::generate_planet(&floptle_field::procgen::PlanetFill {
            seed: 11,
            radius,
            voxel: 1.5,
            relief: radius * 0.065,
            cave_depth: radius * 0.35,
            core_r: 10.5,
            ..Default::default()
        });
        let golden = std::f32::consts::PI * (3.0 - 5f32.sqrt());
        // Down at ground no steeper than about 30°, where a script places
        // things; a ray at a cliff meshes the chunk as it always did.
        let rays: Vec<(Vec3, Vec3)> = (0..1000)
            .map(|i| {
                let y = 1.0 - 2.0 * (i as f32 + 0.5) / 1000.0;
                let r = (1.0 - y * y).sqrt();
                let d = Vec3::new(r * (golden * i as f32).cos(), y, r * (golden * i as f32).sin());
                (d * (radius * 1.3), -d)
            })
            .filter(|(o, d)| field.raycast(*o, *d, radius).is_some_and(|p| field.grad(p).normalize_or_zero().dot(-*d) > 0.87))
            // A chunk apiece: the first rays into a chunk are the ones in question.
            .filter({
                let mut seen = std::collections::HashSet::new();
                let field = &field;
                move |(o, d)| {
                    let p = field.raycast(*o, *d, radius).unwrap_or(Vec3::ZERO);
                    seen.insert((p / (field.voxel() * floptle_field::CHUNK as f32)).floor().to_array().map(|v| v as i32))
                }
            })
            .take(60)
            .collect();
        assert_eq!(rays.len(), 60);
        let cast = |cols: &[crate::AnchoredCollider]| {
            let t = std::time::Instant::now();
            let hits: Vec<Option<f32>> = rays.iter().map(|(o, d)| crate::raycast_colliders(cols, *o, *d, radius, !0).map(|h| h.distance)).collect();
            (hits, t.elapsed().as_secs_f64())
        };
        // Meshed where the rays land: the cost a ray pays at ground a body has
        // stood on.
        let meshed = ChunkTerrain::new(field.clone());
        for (o, d) in &rays {
            if let Some(p) = field.raycast(*o, *d, radius) {
                let _ = meshed.distance(p);
            }
        }
        let meshed = vec![crate::AnchoredCollider::world(Box::new(meshed))];
        let (want, mut built) = cast(&meshed);
        // A fresh collider each time, since the first rays into a chunk are
        // the subject, alternating with the meshed one and keeping each side's
        // fastest: interference on a shared machine only ever adds time, and
        // one slow reading read 9.2x on a release runner.
        let mut got = Vec::new();
        let mut cold = f64::MAX;
        for _ in 0..4 {
            let fresh = vec![crate::AnchoredCollider::world(Box::new(ChunkTerrain::new(field.clone())))];
            let (hits, t) = cast(&fresh);
            cold = cold.min(t);
            got = hits;
            built = built.min(cast(&meshed).1);
        }
        for (i, (a, b)) in got.iter().zip(&want).enumerate() {
            match (a, b) {
                (Some(a), Some(b)) => assert!((a - b).abs() < 0.05, "ray {i}: {a} against {b} on meshed ground"),
                _ => assert_eq!(a.is_some(), b.is_some(), "ray {i}: {a:?} against {b:?}"),
            }
        }
        // About 10x in a debug build, 11x with every core busy, since a ray at
        // meshed ground got cheaper (it was 6x); meshing each chunk a ray
        // reaches, the cost this guards, read 280x.
        assert!(cold < 40.0 * built, "60 rays at fresh ground took {:.2} ms, at meshed ground {:.2} ms", cold * 1e3, built * 1e3);
    }

    /// A terrain collider that leaves every ray to the general march, as all
    /// terrain did: the reference the field-first ray is checked against.
    struct MarchOnly(ChunkTerrain);

    impl CollisionShape for MarchOnly {
        fn distance(&self, p: Vec3) -> f32 {
            self.0.distance(p)
        }
        fn normal(&self, p: Vec3) -> Vec3 {
            self.0.normal(p)
        }
        fn ray_normal(&self, p: Vec3, rd: Vec3) -> Vec3 {
            self.0.ray_normal(p, rd)
        }
    }

    /// **A ray that misses a planet costs about what one that hits it does, and
    /// both find what the march found.** 124 m rays, a thousand straight down
    /// at the ground and a thousand skimming past above it. The march stepped
    /// a metre at a time with a closest-triangle search at every step, so a
    /// miss walked its whole length that way: ten times a hit.
    #[test]
    fn a_ray_past_a_planet_costs_about_what_a_ray_into_it_does() {
        let radius = 150.0;
        let field = floptle_field::procgen::generate_planet(&floptle_field::procgen::PlanetFill {
            seed: 11,
            radius,
            voxel: 1.5,
            relief: radius * 0.065,
            cave_depth: radius * 0.35,
            core_r: 10.5,
            ..Default::default()
        });
        let fast = vec![crate::AnchoredCollider::world(Box::new(ChunkTerrain::new(field.clone())))];
        let slow = vec![crate::AnchoredCollider::world(Box::new(MarchOnly(ChunkTerrain::new(field))))];
        let mut r = floptle_core::noise::Rng::new(5);
        let (mut down, mut past) = (Vec::new(), Vec::new());
        for _ in 0..1000 {
            let d = Vec3::new(r.range(-1.0, 1.0) as f32, r.range(-1.0, 1.0) as f32, r.range(-1.0, 1.0) as f32).normalize_or_zero();
            if d == Vec3::ZERO {
                continue;
            }
            let side = d.cross(if d.y.abs() < 0.9 { Vec3::Y } else { Vec3::X }).normalize();
            down.push((d * (radius + 30.0), -d));
            past.push((d * (radius + 25.0) - side * 62.0, side));
        }
        let cast = |cols: &[crate::AnchoredCollider], rays: &[(Vec3, Vec3)]| {
            let t = std::time::Instant::now();
            let hits: Vec<Option<f32>> = rays.iter().map(|(o, d)| crate::raycast_colliders(cols, *o, *d, 124.0, !0).map(|h| h.distance)).collect();
            (hits, t.elapsed().as_secs_f64())
        };
        // Warm both caches, so the timing is the ray and not first meshing.
        cast(&fast, &down);
        cast(&slow, &down);
        let (hit_fast, hit_s) = cast(&fast, &down);
        let (miss_fast, miss_s) = cast(&fast, &past);
        let (hit_slow, _) = cast(&slow, &down);
        let (miss_slow, _) = cast(&slow, &past);
        for (i, (a, b)) in hit_fast.iter().zip(&hit_slow).enumerate() {
            match (a, b) {
                (Some(a), Some(b)) => assert!((a - b).abs() < 0.05, "ray {i} down: {a} against the march's {b}"),
                _ => assert_eq!(a.is_some(), b.is_some(), "ray {i} down: {a:?} against the march's {b:?}"),
            }
        }
        assert_eq!(miss_fast, miss_slow, "the rays past the planet answer what the march answered");
        assert!(hit_fast.iter().filter(|h| h.is_some()).count() > 950, "the rays down did not find the ground");
        assert!(miss_fast.iter().filter(|h| h.is_none()).count() > 950, "the rays past it were not misses");
        println!("1000 rays of 124 m: down {:.3} ms each, past {:.3} ms each", hit_s, miss_s);
        assert!(miss_s < 2.0 * hit_s, "a miss costs {:.1}x a hit", miss_s / hit_s);
    }
}

#[cfg(test)]
mod narrowed_query_tests {
    //! The narrowed terrain and mesh queries answer what the full ones do.
    use super::*;
    use floptle_field::{Brush, BrushProfile, ChunkField};

    fn rough() -> ChunkField {
        let mut f = ChunkField::new(0.5);
        f.fill_slab(Vec3::new(-14.0, -3.0, -14.0), Vec3::new(14.0, 0.0, 14.0), 0.0, [0.5; 3]);
        let mut seed = 777u32;
        let mut rnd = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as f32 / 16777216.0
        };
        for _ in 0..40 {
            let c = Vec3::new(rnd() * 20.0 - 10.0, rnd() * 2.0 - 0.5, rnd() * 20.0 - 10.0);
            let b = if rnd() > 0.4 { Brush::Raise } else { Brush::Lower };
            f.sculpt(b, c, 1.0 + rnd() * 2.0, 1.0, BrushProfile::default());
        }
        f
    }

    fn rnd_from(mut seed: u64) -> impl FnMut() -> f32 {
        move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 40) as f32 / (1u64 << 24) as f32
        }
    }

    /// The drawn-surface search as it was: every bucket of every ring, every
    /// triangle tested, from the hashed chunks.
    fn closest_reference(t: &ChunkTerrain, local: Vec3) -> Option<(Vec3, Vec3)> {
        if t.field.d(local).abs() >= t.field.band() {
            return None;
        }
        let mut s = t.surface.borrow_mut();
        t.ensure_meshed(local, &mut s);
        let cell = t.bucket();
        let c = cell_coord(local, cell);
        let mut best = f32::INFINITY;
        let mut hit: Option<(Vec3, Vec3)> = None;
        let max_ring = (t.reach() / cell).ceil().max(1.0) as i32;
        for r in 0..=max_ring {
            for cz in c.2 - r..=c.2 + r {
                for cy in c.1 - r..=c.1 + r {
                    for cx in c.0 - r..=c.0 + r {
                        if (cx - c.0).abs() != r && (cy - c.1).abs() != r && (cz - c.2).abs() != r {
                            continue;
                        }
                        s.each_in((cx, cy, cz), |tri| {
                            let q = closest_point_on_triangle(local, tri.p[0], tri.p[1], tri.p[2]);
                            let d2 = (q - local).length_squared();
                            if d2 < best {
                                best = d2;
                                hit = Some((q, normal_at(tri, q)));
                            }
                        });
                    }
                }
            }
            if hit.is_some() {
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

    /// The field's trilinear read from one voxel at a time, as it was.
    fn d_reference(f: &ChunkField, p: Vec3) -> f32 {
        let g = p / f.voxel();
        let b = [g.x.floor() as i32, g.y.floor() as i32, g.z.floor() as i32];
        let fr = Vec3::new(g.x - b[0] as f32, g.y - b[1] as f32, g.z - b[2] as f32);
        let c = |dx: i32, dy: i32, dz: i32| f.voxel_at([b[0] + dx, b[1] + dy, b[2] + dz]);
        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let x00 = lerp(c(0, 0, 0), c(1, 0, 0), fr.x);
        let x10 = lerp(c(0, 1, 0), c(1, 1, 0), fr.x);
        let x01 = lerp(c(0, 0, 1), c(1, 0, 1), fr.x);
        let x11 = lerp(c(0, 1, 1), c(1, 1, 1), fr.x);
        lerp(lerp(x00, x10, fr.y), lerp(x01, x11, fr.y), fr.z)
    }

    /// Over rough drawn ground, inside it and above it, across chunk
    /// borders, the narrowed search finds the same point and the same normal
    /// to the bit as the full walk, and the field reads what it read voxel
    /// by voxel.
    #[test]
    fn the_narrowed_surface_search_answers_what_the_full_walk_does() {
        let t = ChunkTerrain::new(rough());
        let mut rnd = rnd_from(0x9e37_79b9_7f4a_7c15);
        let mut probes: Vec<Vec3> = (0..5000)
            .map(|_| Vec3::new(rnd() * 26.0 - 13.0, rnd() * 5.0 - 2.5, rnd() * 26.0 - 13.0))
            .collect();
        // Along a chunk face (16 m at half-metre voxels) and its corner.
        probes.extend((0..200).map(|k| Vec3::new(16.0 - 0.003 * k as f32, 0.2 + 0.01 * k as f32, -0.5)));
        // In open air: in chunks nothing was ever written to, and past the
        // field's edge, where the field reads the band from no chunk at all.
        probes.extend((0..400).map(|_| Vec3::new(rnd() * 80.0 - 40.0, rnd() * 30.0 - 5.0, rnd() * 80.0 - 40.0)));
        let bits = |r: Option<(Vec3, Vec3)>| r.map(|(q, n)| (q.to_array().map(f32::to_bits), n.to_array().map(f32::to_bits)));
        let mut found = 0;
        for p in probes {
            assert_eq!(t.field.d(p).to_bits(), d_reference(&t.field, p).to_bits(), "the field reads differently at {p}");
            let want = closest_reference(&t, p);
            let got = t.closest_surface(p);
            assert_eq!(bits(got), bits(want), "at {p}: the narrowed search found another point");
            found += usize::from(got.is_some());
        }
        assert!(found > 2000, "only {found} probes were near the surface: the comparison proved little");
    }

    /// A query in the air over flat ground tests a few triangles, not the
    /// several hundred in the buckets between it and the ground.
    #[test]
    fn a_query_over_the_ground_tests_a_handful_of_triangles() {
        let mut f = ChunkField::new(0.75);
        f.fill_slab(Vec3::new(-30.0, -6.0, -30.0), Vec3::new(30.0, 0.0, 30.0), 0.0, [0.5; 3]);
        let t = ChunkTerrain::new(f);
        let mut rnd = rnd_from(42);
        let mut worst = 0;
        for h in [0.3f32, 1.2, 2.0, 2.8] {
            let points: Vec<Vec3> = (0..200).map(|_| Vec3::new(rnd() * 30.0 - 15.0, h, rnd() * 30.0 - 15.0)).collect();
            for &p in &points {
                t.distance(p); // meshed first, so meshing is not counted
            }
            TERRAIN_TRIS.with(|n| n.set(0));
            for &p in &points {
                t.distance(p);
            }
            let per = TERRAIN_TRIS.with(|n| n.get()) / points.len() as u64;
            eprintln!("{h} m up: {per} triangles a query");
            worst = worst.max(per);
        }
        assert!(worst <= 24, "a query over flat ground tested {worst} triangles");
    }

    /// Wherever the field promises open air over a box, the distance at every
    /// point of the box is at least what it promised; and over open air well
    /// above the ground it does promise, or the promise saves nothing.
    #[test]
    fn a_box_called_clear_of_the_terrain_is() {
        let t = ChunkTerrain::posed(rough(), Quat::from_rotation_z(0.2), 1.5);
        let mut rnd = rnd_from(7);
        let (mut clear, mut high) = (0, 0);
        for _ in 0..3000 {
            let c = Vec3::new(rnd() * 30.0 - 15.0, rnd() * 24.0 - 4.0, rnd() * 30.0 - 15.0);
            let half = Vec3::new(rnd(), rnd(), rnd()) * 2.0;
            let r = rnd() * 0.8;
            let ok = t.clear_of(c - half, c + half, r);
            if c.y - half.y > 14.0 && c.x.abs() < 6.0 && c.z.abs() < 6.0 {
                high += 1;
                clear += usize::from(ok);
            }
            if ok {
                for _ in 0..12 {
                    let p = c + (Vec3::new(rnd(), rnd(), rnd()) * 2.0 - Vec3::ONE) * half;
                    let d = t.distance(p);
                    assert!(d >= r, "called clear of {r} at {p}, distance {d}");
                }
            }
        }
        assert!(high > 20 && clear == high, "only {clear} of {high} boxes high over the ground were called clear");
    }

    /// A mesh asked whether a point is within `r` answers the full distance
    /// to the bit wherever it is, and at least `r` wherever it is not.
    #[test]
    fn a_bounded_mesh_distance_is_the_distance_where_it_matters() {
        let (mut verts, mut idx) = (Vec::new(), Vec::new());
        let n = 30u32;
        for z in 0..=n {
            for x in 0..=n {
                let (fx, fz) = (x as f32 * 0.6 - 9.0, z as f32 * 0.6 - 9.0);
                verts.push(Vec3::new(fx, 0.4 * (fx * 0.5).sin() * (fz * 0.4).cos(), fz));
            }
        }
        for z in 0..n {
            for x in 0..n {
                let a = z * (n + 1) + x;
                idx.extend([a, a + 1, a + n + 1, a + 1, a + n + 2, a + n + 1]);
            }
        }
        let m = TriMeshCollider::new(&verts, &idx);
        let mut rnd = rnd_from(99);
        let mut under = 0;
        for k in 0..20000 {
            let p = Vec3::new(rnd() * 24.0 - 12.0, rnd() * 6.0 - 2.0, rnd() * 24.0 - 12.0);
            let r = if k % 50 == 0 { 0.0 } else { rnd() * 3.0 };
            let full = m.distance(p);
            let got = m.distance_within(p, r);
            if full < r {
                assert_eq!(got.to_bits(), full.to_bits(), "at {p} within {r}: {got} for {full}");
                under += 1;
            } else {
                assert!(got >= r, "at {p} within {r}: {got} although the distance is {full}");
            }
        }
        assert!(under > 3000, "only {under} probes were within their radius");
    }
}
