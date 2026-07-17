//! CPU mesh residency + ray-triangle picking for the vertex-paint brush.
//!
//! Two gaps this closes, both real before it existed:
//!
//! 1. **The editor retains no CPU geometry.** `MeshAsset` holds only `MeshId`s, and
//!    every consumer that needs vertices (`play.rs`, `net.rs`, `viz.rs`,
//!    `terrain_edit.rs`) re-imports the `.glb` from disk. A brush cannot re-import per
//!    dab, so painted meshes get a retained cache here.
//! 2. **Nothing in the engine casts a ray at a triangle.** `pick()` tests analytic
//!    primitives; `TriMeshCollider` is an unsigned closest-point spatial hash. So this
//!    adds Möller–Trumbore plus an acceleration structure — deliberately the SAME
//!    uniform-spatial-hash shape `TriMeshCollider` uses, ray-walked instead of
//!    sphere-queried, rather than introducing a second spatial-structure concept.
//!
//! ## Three bounds that are load-bearing, not defensive
//!
//! The first cut of this froze the editor outright. Every one of these has a regression
//! test below; if you touch the traversal, keep them:
//!
//! 1. **`raycast` clips to the part's AABB before marching.** It used to walk `t = 0`
//!    to `max_t` (the brush passes `1e5`) at half-cell steps — on a mesh with millimetre
//!    triangles that is tens of millions of iterations, per mesh, per frame. A MISS is
//!    the common case (most of the scene isn't under the cursor) and must cost one slab
//!    test.
//! 2. **`build` sends huge triangles to `oversized` instead of bucketing them.** Cell
//!    size comes from the MEAN edge, so one big floor quad among fine detail spans
//!    `(extent/cell)³` cells. This is not hypothetical: `RetroMap.glb` has exactly one
//!    such triangle. `cell` is also floored at `extent/MAX_CELLS_PER_AXIS`.
//! 3. **`in_radius` falls back to a linear scan** when the cell sweep `(2r+1)³` would
//!    cost more than just walking the vertices. A 0.5 brush on a 1mm mesh is otherwise
//!    ~8M lookups per dab.

use std::collections::HashMap;

use floptle_core::math::{Vec2, Vec3};
use floptle_render::{MeshData, Vertex};

/// A ray hit on ONE part (before the part index is known).
#[derive(Clone, Copy)]
struct PartHit {
    t: f32,
    pos: Vec3,
    normal: Vec3,
}

/// A ray hit on a mesh part. The brush only needs WHERE (`pos`) and on WHICH node/part —
/// texture paint works in world space off the atlas geometry, not the hit's UV/triangle.
pub(crate) struct MeshHit {
    /// Distance along the ray.
    pub(crate) t: f32,
    /// Hit position in the mesh's local space.
    pub(crate) pos: Vec3,
    /// Geometric normal of the struck triangle (local space, unnormalized winding).
    pub(crate) normal: Vec3,
    /// Which part of the asset was struck.
    pub(crate) part: usize,
}

/// One part's CPU geometry plus its triangle grid.
pub(crate) struct PaintPart {
    pub(crate) verts: Vec<Vertex>,
    indices: Vec<u32>,
    /// Imported per-vertex colors (glTF COLOR_0), parallel to `verts` — retained so a
    /// texture-paint atlas can carry the mesh's painted look over to its unshared vertices.
    colors: Option<Vec<[u8; 4]>>,
    /// Uniform grid: cell → triangle indices (by triangle number).
    grid: HashMap<[i32; 3], Vec<u32>>,
    cell: f32,
    /// Triangles whose AABB spans too many cells to bucket sanely (a ground quad
    /// among fine detail). They are tested on EVERY raycast instead. Without this
    /// escape hatch, one big triangle in a finely-tessellated mesh registers into
    /// `(extent/cell)³` cells — which is how `build` turns into a hang.
    oversized: Vec<u32>,
    /// The part's bounds. A ray is clipped to this before marching; a miss then costs
    /// one slab test instead of a walk to `max_t`.
    min: Vec3,
    max: Vec3,
    /// Vertex positions bucketed the same way, so a brush dab finds the vertices in
    /// its radius without scanning the whole mesh.
    vgrid: HashMap<[i32; 3], Vec<u32>>,
}

/// Padding, in texels, around every triangle's atlas rect. Two jobs: it keeps a
/// triangle's texels away from its neighbour's (nearest sampling means no bleed, but the
/// gap is cheap insurance), and it absorbs the ~1-texel over-fill the brush uses to close
/// cross-triangle seams (see [`for_each_cell_texel`]) so that over-fill never lands in the
/// next triangle's cell.
const ATLAS_PAD: u32 = 2;
/// Target texels per triangle on AVERAGE — the paint texture's resolution knob. The real
/// per-triangle budget is redistributed by world-space area (a big/stretched face gets
/// proportionally more), which is what keeps texel density — and so the painted detail —
/// uniform across faces regardless of their shape. See [`PaintMeshCache::atlas_mesh`].
const PX_PER_TRI: f32 = 22.0;
const MAX_ATLAS_EDGE: u32 = 2048;
const MIN_ATLAS_EDGE: u32 = 128;

/// One triangle's slot in the paint atlas: the atlas UVs of its three vertices (matching
/// the render mesh), their object-space positions (for the world-space brush falloff), and
/// their original mesh UVs.
#[derive(Clone)]
pub(crate) struct AtlasCell {
    pub(crate) uv: [[f32; 2]; 3],
    pub(crate) pos: [[f32; 3]; 3],
    /// The original mesh UVs — unused by the overlay render (the base draws itself), but
    /// kept so the forward-render tests can model the base pass exactly.
    #[allow(dead_code)]
    pub(crate) src_uv: [[f32; 2]; 3],
}

/// A part's paint atlas: the unique-UV render mesh, the texture edge it was laid out for,
/// and one [`AtlasCell`] per triangle.
pub(crate) struct MeshAtlas {
    pub(crate) mesh: MeshData,
    pub(crate) edge: u32,
    pub(crate) cells: Vec<AtlasCell>,
    /// The ORIGINAL vertex id behind each atlas vertex (atlas vertices are unshared, three
    /// per triangle, so this is the part's index list in order). This is what lets a painted
    /// node keep its per-vertex colors: the node's vertex-paint block is remapped through it
    /// into an atlas-ordered mirror block (see `paint_tex`'s mirror sync).
    pub(crate) orig_vids: Vec<u32>,
}

/// Visit every texel the render mesh can sample for this triangle. Calls
/// `f(pixel_byte_index, [wa, wb, wc])` with the barycentric weights of the three vertices.
///
/// This is the seam fix. A brush dab paints in WORLD space — for each texel it reconstructs
/// the surface point (`wa·p0 + wb·p1 + wc·p2`) and weights by world distance to the cursor.
/// Two texels on either side of a shared edge reconstruct (nearly) the same world point, so
/// they get the same colour: the paint flows across the edge with no visible seam, even
/// though the two triangles live in different corners of the atlas.
///
/// The triangle is walked DILATED by one texel per edge (the per-edge bias), so the last
/// rendered texel right at the edge is always covered — an exact point-in-triangle test can
/// leave a one-texel gap there that would flash the seed texture through. The over-fill is
/// bounded to one texel, which [`ATLAS_PAD`] keeps inside this triangle's own rect.
pub(crate) fn for_each_cell_texel(cell: &AtlasCell, edge: u32, mut f: impl FnMut(usize, [f32; 3])) {
    let e = edge as f32;
    let a = Vec2::new(cell.uv[0][0] * e, cell.uv[0][1] * e);
    let b = Vec2::new(cell.uv[1][0] * e, cell.uv[1][1] * e);
    let c = Vec2::new(cell.uv[2][0] * e, cell.uv[2][1] * e);
    let det = (b.y - c.y) * (a.x - c.x) + (c.x - b.x) * (a.y - c.y);
    if det.abs() < 1e-9 {
        return; // degenerate cell — nothing to sample
    }
    let inv = 1.0 / det;
    let area2 = det.abs();
    // One-texel bias per edge: distance→barycentric scale is |opposite edge| / (2·area).
    let bias0 = (b - c).length() / area2; // edge opposite a
    let bias1 = (c - a).length() / area2; // edge opposite b
    let bias2 = (a - b).length() / area2; // edge opposite c
    let lo_x = (a.x.min(b.x).min(c.x) - 1.0).floor().max(0.0) as u32;
    let hi_x = (a.x.max(b.x).max(c.x) + 1.0).ceil().min(e - 1.0) as u32;
    let lo_y = (a.y.min(b.y).min(c.y) - 1.0).floor().max(0.0) as u32;
    let hi_y = (a.y.max(b.y).max(c.y) + 1.0).ceil().min(e - 1.0) as u32;
    for ty in lo_y..=hi_y {
        for tx in lo_x..=hi_x {
            let px = tx as f32 + 0.5;
            let py = ty as f32 + 0.5;
            let l0 = ((b.y - c.y) * (px - c.x) + (c.x - b.x) * (py - c.y)) * inv;
            let l1 = ((c.y - a.y) * (px - c.x) + (a.x - c.x) * (py - c.y)) * inv;
            let l2 = 1.0 - l0 - l1;
            if l0 < -bias0 || l1 < -bias1 || l2 < -bias2 {
                continue;
            }
            f(((ty * edge + tx) * 4) as usize, [l0, l1, l2]);
        }
    }
}

/// Shelf-pack axis-aligned rects (texel `[w, h]`) into the smallest power-of-two square
/// (≥ [`MIN_ATLAS_EDGE`], ≤ `max_edge`) that holds them. Returns `(edge, origins)` or
/// `None` if they don't fit even at `max_edge`. Deterministic (a stable height sort), so a
/// reload rebuilds the identical layout — which is what lets saved paint pixels line up.
fn shelf_pack(rects: &[[u32; 2]], max_edge: u32) -> Option<(u32, Vec<[u32; 2]>)> {
    let area: u64 = rects.iter().map(|r| r[0] as u64 * r[1] as u64).sum();
    let mut edge = MIN_ATLAS_EDGE;
    // Start near the area lower bound (×10/7 for shelf waste) so we don't retry from tiny.
    while (edge as u64 * edge as u64) < area * 10 / 7 && edge < max_edge {
        edge <<= 1;
    }
    loop {
        if let Some(places) = try_shelf(rects, edge) {
            return Some((edge, places));
        }
        if edge >= max_edge {
            return None;
        }
        edge <<= 1;
    }
}

fn try_shelf(rects: &[[u32; 2]], edge: u32) -> Option<Vec<[u32; 2]>> {
    let mut order: Vec<usize> = (0..rects.len()).collect();
    order.sort_by(|&a, &b| rects[b][1].cmp(&rects[a][1])); // tallest first (stable)
    let mut places = vec![[0u32; 2]; rects.len()];
    let (mut sx, mut sy, mut shelf_h) = (0u32, 0u32, 0u32);
    for &i in &order {
        let [w, h] = rects[i];
        if w > edge {
            return None; // a single rect wider than the atlas
        }
        if sx + w > edge {
            sy += shelf_h; // wrap to a new shelf
            sx = 0;
            shelf_h = 0;
        }
        if sy + h > edge {
            return None; // ran off the bottom
        }
        places[i] = [sx, sy];
        sx += w;
        shelf_h = shelf_h.max(h);
    }
    Some(places)
}

/// Cap on how many cells one triangle may register into before it is treated as
/// oversized. Small: the point is to bound the worst case, not to bucket perfectly.
const MAX_CELLS_PER_TRI: i64 = 64;
/// Cap on grid resolution along the longest axis. Bounds both memory and the march.
const MAX_CELLS_PER_AXIS: f32 = 128.0;

impl PaintPart {
    fn build(data: &MeshData) -> Self {
        let tris = data.indices.len() / 3;
        let mut min = Vec3::splat(f32::INFINITY);
        let mut max = Vec3::splat(f32::NEG_INFINITY);
        for v in &data.vertices {
            let p = Vec3::from(v.pos);
            min = min.min(p);
            max = max.max(p);
        }
        if data.vertices.is_empty() {
            min = Vec3::ZERO;
            max = Vec3::ZERO;
        }

        // Cell size from the mean triangle edge — but FLOORED so the grid can never be
        // finer than MAX_CELLS_PER_AXIS across the mesh. Mean edge alone is a trap: one
        // big triangle among fine ones gives a tiny cell and an enormous grid.
        let mut acc = 0f64;
        for t in 0..tris {
            let a = Vec3::from(data.vertices[data.indices[t * 3] as usize].pos);
            let b = Vec3::from(data.vertices[data.indices[t * 3 + 1] as usize].pos);
            acc += (b - a).length() as f64;
        }
        let extent = (max - min).max_element().max(1e-4);
        let mean_edge = if tris > 0 { (acc / tris as f64) as f32 } else { extent };
        let cell = mean_edge.max(extent / MAX_CELLS_PER_AXIS).max(1e-4);

        let mut grid: HashMap<[i32; 3], Vec<u32>> = HashMap::new();
        let mut oversized = Vec::new();
        for t in 0..tris {
            let a = Vec3::from(data.vertices[data.indices[t * 3] as usize].pos);
            let b = Vec3::from(data.vertices[data.indices[t * 3 + 1] as usize].pos);
            let c = Vec3::from(data.vertices[data.indices[t * 3 + 2] as usize].pos);
            let lo = cell_of(a.min(b).min(c), cell);
            let hi = cell_of(a.max(b).max(c), cell);
            let span = (hi[0] - lo[0] + 1) as i64
                * (hi[1] - lo[1] + 1) as i64
                * (hi[2] - lo[2] + 1) as i64;
            if span > MAX_CELLS_PER_TRI {
                oversized.push(t as u32);
                continue;
            }
            for x in lo[0]..=hi[0] {
                for y in lo[1]..=hi[1] {
                    for z in lo[2]..=hi[2] {
                        grid.entry([x, y, z]).or_default().push(t as u32);
                    }
                }
            }
        }
        let mut vgrid: HashMap<[i32; 3], Vec<u32>> = HashMap::new();
        for (i, v) in data.vertices.iter().enumerate() {
            vgrid.entry(cell_of(Vec3::from(v.pos), cell)).or_default().push(i as u32);
        }
        Self {
            verts: data.vertices.clone(),
            indices: data.indices.clone(),
            colors: data.colors.clone(),
            grid,
            cell,
            oversized,
            min,
            max,
            vgrid,
        }
    }

    fn tri(&self, t: u32) -> (Vec3, Vec3, Vec3) {
        let i = t as usize * 3;
        (
            Vec3::from(self.verts[self.indices[i] as usize].pos),
            Vec3::from(self.verts[self.indices[i + 1] as usize].pos),
            Vec3::from(self.verts[self.indices[i + 2] as usize].pos),
        )
    }

    /// The triangle's three vertex UVs.
    fn tri_uv(&self, t: u32) -> ([f32; 2], [f32; 2], [f32; 2]) {
        let i = t as usize * 3;
        (
            self.verts[self.indices[i] as usize].uv,
            self.verts[self.indices[i + 1] as usize].uv,
            self.verts[self.indices[i + 2] as usize].uv,
        )
    }

    fn test(&self, t: u32, ro: Vec3, rd: Vec3, max_t: f32, best: &mut Option<PartHit>) {
        let (a, b, c) = self.tri(t);
        if let Some(h) = ray_tri(ro, rd, a, b, c)
            && h < max_t
            && best.is_none_or(|b| h < b.t)
        {
            *best = Some(PartHit {
                t: h,
                pos: ro + rd * h,
                normal: (b - a).cross(c - a).normalize_or_zero(),
            });
        }
    }

    /// Nearest ray hit, or `None`. `ro`/`rd` are in the mesh's local space.
    fn raycast(&self, ro: Vec3, rd: Vec3, max_t: f32) -> Option<PartHit> {
        // Clip to the part's bounds FIRST. This is what makes the cost proportional to
        // the mesh rather than to `max_t`: a ray that misses (the common case — most of
        // the scene isn't under the cursor) pays one slab test and leaves.
        let (mut t0, t1) = ray_aabb(ro, rd, self.min - self.cell, self.max + self.cell)?;
        t0 = t0.max(0.0);
        let t1 = t1.min(max_t);
        if t0 > t1 {
            return None;
        }

        let mut best: Option<PartHit> = None;
        for &t in &self.oversized {
            self.test(t, ro, rd, max_t, &mut best);
        }

        // March only the clipped span, and only re-test when the cell actually changes.
        let step = self.cell * 0.5;
        let mut last: Option<[i32; 3]> = None;
        let mut t = t0;
        loop {
            let c = cell_of(ro + rd * t, self.cell);
            if last != Some(c) {
                last = Some(c);
                // 3×3×3: a triangle can sit in an adjacent cell and still be pierced by
                // a ray passing through this one.
                for dx in -1..=1 {
                    for dy in -1..=1 {
                        for dz in -1..=1 {
                            if let Some(list) = self.grid.get(&[c[0] + dx, c[1] + dy, c[2] + dz]) {
                                for &tri in list {
                                    self.test(tri, ro, rd, max_t, &mut best);
                                }
                            }
                        }
                    }
                }
            }
            // A hit behind us can't be beaten by anything further along.
            if let Some(b) = &best
                && b.t < t
            {
                break;
            }
            if t >= t1 {
                break;
            }
            t = (t + step).min(t1);
        }
        best
    }

    /// Every vertex within `radius` of `p` (local space), as `(index, distance)`.
    fn in_radius(&self, p: Vec3, radius: f32) -> Vec<(u32, f32)> {
        let hit = |i: u32, out: &mut Vec<(u32, f32)>| {
            let d = (Vec3::from(self.verts[i as usize].pos) - p).length();
            if d <= radius {
                out.push((i, d));
            }
        };
        let mut out = Vec::new();
        let r = (radius / self.cell).ceil().max(0.0);
        // A big brush on a fine mesh sweeps (2r+1)³ cells — millions of lookups for a
        // radius the mesh's own vertex count would answer in one pass. Take whichever
        // is cheaper; both give identical results.
        let cells = (2.0 * r + 1.0).powi(3);
        if !cells.is_finite() || cells > self.verts.len() as f32 {
            for i in 0..self.verts.len() as u32 {
                hit(i, &mut out);
            }
            return out;
        }
        let r = r as i32;
        let c = cell_of(p, self.cell);
        for x in -r..=r {
            for y in -r..=r {
                for z in -r..=r {
                    if let Some(list) = self.vgrid.get(&[c[0] + x, c[1] + y, c[2] + z]) {
                        for &i in list {
                            hit(i, &mut out);
                        }
                    }
                }
            }
        }
        out
    }
}

/// Ray vs axis-aligned box (slab test) → the `(enter, exit)` ray parameters, or `None`.
/// Handles axis-parallel rays: a zero component yields ±inf bounds, which the min/max
/// fold absorbs — no division-by-zero special case needed.
fn ray_aabb(ro: Vec3, rd: Vec3, min: Vec3, max: Vec3) -> Option<(f32, f32)> {
    let inv = Vec3::ONE / rd;
    let a = (min - ro) * inv;
    let b = (max - ro) * inv;
    let lo = a.min(b);
    let hi = a.max(b);
    let t0 = lo.max_element();
    let t1 = hi.min_element();
    (t1 >= t0.max(0.0)).then_some((t0, t1))
}

fn cell_of(p: Vec3, cell: f32) -> [i32; 3] {
    [
        (p.x / cell).floor() as i32,
        (p.y / cell).floor() as i32,
        (p.z / cell).floor() as i32,
    ]
}

/// Möller–Trumbore. Returns the ray parameter of the hit; two-sided, because painting
/// the inside of a shell is legitimate.
fn ray_tri(ro: Vec3, rd: Vec3, a: Vec3, b: Vec3, c: Vec3) -> Option<f32> {
    let e1 = b - a;
    let e2 = c - a;
    let h = rd.cross(e2);
    let det = e1.dot(h);
    if det.abs() < 1e-8 {
        return None; // ray parallel to the triangle plane
    }
    let inv = 1.0 / det;
    let s = ro - a;
    let u = inv * s.dot(h);
    if !(-1e-5..=1.000_01).contains(&u) {
        return None;
    }
    let q = s.cross(e1);
    let v = inv * rd.dot(q);
    if v < -1e-5 || u + v > 1.000_01 {
        return None;
    }
    let t = inv * e2.dot(q);
    (t > 1e-4).then_some(t)
}

/// Retained CPU geometry for painted meshes, keyed exactly like `mesh_registry`
/// (asset path) so the two stay in step. Built lazily — a project full of models pays
/// only for the ones actually painted.
#[derive(Default)]
pub(crate) struct PaintMeshCache {
    parts: HashMap<String, Vec<PaintPart>>,
}

impl PaintMeshCache {
    /// Geometry for `key`, building it from `data` on first use.
    pub(crate) fn get_or_build<'a>(
        &'a mut self,
        key: &str,
        data: impl FnOnce() -> Vec<MeshData>,
    ) -> &'a Vec<PaintPart> {
        if !self.parts.contains_key(key) {
            let built: Vec<PaintPart> = data().iter().map(PaintPart::build).collect();
            self.parts.insert(key.to_string(), built);
        }
        &self.parts[key]
    }

    pub(crate) fn get(&self, key: &str) -> Option<&Vec<PaintPart>> {
        self.parts.get(key)
    }

    /// Drop every cached model. Called wherever `mesh_registry` is cleared (project
    /// open, scene reload) — geometry the renderer has dropped must not linger here, or
    /// the brush would raycast a stale mesh and paint the wrong vertices.
    pub(crate) fn clear(&mut self) {
        self.parts.clear();
    }

    /// Nearest hit across all of a model's parts.
    pub(crate) fn raycast(&self, key: &str, ro: Vec3, rd: Vec3, max_t: f32) -> Option<MeshHit> {
        let parts = self.parts.get(key)?;
        let mut best: Option<MeshHit> = None;
        for (i, part) in parts.iter().enumerate() {
            if let Some(h) = part.raycast(ro, rd, max_t)
                && best.as_ref().is_none_or(|b| h.t < b.t)
            {
                best = Some(MeshHit { t: h.t, pos: h.pos, normal: h.normal, part: i });
            }
        }
        best
    }

    pub(crate) fn in_radius(&self, key: &str, part: usize, p: Vec3, radius: f32) -> Vec<(u32, f32)> {
        self.parts
            .get(key)
            .and_then(|ps| ps.get(part))
            .map(|pp| pp.in_radius(p, radius))
            .unwrap_or_default()
    }

    pub(crate) fn vertex_count(&self, key: &str, part: usize) -> u32 {
        self.parts
            .get(key)
            .and_then(|ps| ps.get(part))
            .map_or(0, |p| p.verts.len() as u32)
    }

    /// A part's local-space bounds — the brush's cheap "does the sphere even touch this
    /// node" reject before any per-triangle work.
    pub(crate) fn part_bounds(&self, key: &str, part: usize) -> Option<(Vec3, Vec3)> {
        self.parts.get(key)?.get(part).map(|p| (p.min, p.max))
    }

    /// Build a part's paint ATLAS: a unique-UV render mesh (identical positions + normals)
    /// where every triangle owns its own patch of the texture, packed by its real shape.
    ///
    /// Two properties the old uniform `cols × cols` grid lacked:
    ///   * **No repeating** — each triangle's UVs are unique, so a dab lands in exactly one
    ///     patch even when the mesh's own UVs tile (the reason this exists at all).
    ///   * **Uniform texel density** — every triangle is flattened to its plane, and its
    ///     rect is sized in proportion to that flattened extent at a shared texels-per-unit
    ///     density. A long, thin face therefore gets a long, thin (higher-resolution) rect
    ///     rather than being squashed into a square, so painted detail doesn't stretch.
    ///
    /// The rects are shelf-packed into one power-of-two texture, and each cell also records
    /// its object-space vertex positions + original UVs so the brush can paint in world
    /// space and the canvas can be seeded from the node's existing texture.
    pub(crate) fn atlas_mesh(&self, key: &str, part: usize) -> Option<MeshAtlas> {
        let pp = self.parts.get(key)?.get(part)?;
        let tris = pp.indices.len() / 3;
        if tris == 0 {
            return None;
        }

        // 1. Flatten every triangle to its own plane → a 2D size (object units) plus the
        //    vertices' normalized position inside that size box.
        struct Proj {
            size: [f32; 2],
            n2: [[f32; 2]; 3], // each vertex's position in [0,1]² of the size box
            pos: [[f32; 3]; 3],
            src_uv: [[f32; 2]; 3],
        }
        let mut projs: Vec<Proj> = Vec::with_capacity(tris);
        let mut area_sum = 0.0f32;
        for t in 0..tris {
            let (a, b, c) = pp.tri(t as u32);
            let (ua, ub, uc) = pp.tri_uv(t as u32);
            // In-plane orthonormal basis (x along an edge, y perpendicular in the plane).
            let mut x = b - a;
            if x.length_squared() < 1e-12 {
                x = c - a;
            }
            let x = x.normalize_or_zero();
            let n = (b - a).cross(c - a);
            let mut y = n.cross(x);
            if y.length_squared() < 1e-12 {
                // Degenerate (zero-area) triangle — fabricate a perpendicular so it still
                // gets a (tiny) valid cell rather than poisoning the pack.
                y = if x.x.abs() < 0.9 { Vec3::X.cross(x) } else { Vec3::Y.cross(x) };
            }
            let y = y.normalize_or_zero();
            let p = |v: Vec3| [(v - a).dot(x), (v - a).dot(y)];
            let (p0, p1, p2) = (p(a), p(b), p(c));
            let minx = p0[0].min(p1[0]).min(p2[0]);
            let maxx = p0[0].max(p1[0]).max(p2[0]);
            let miny = p0[1].min(p1[1]).min(p2[1]);
            let maxy = p0[1].max(p1[1]).max(p2[1]);
            let w = (maxx - minx).max(1e-5);
            let h = (maxy - miny).max(1e-5);
            let nrm = |q: [f32; 2]| [(q[0] - minx) / w, (q[1] - miny) / h];
            area_sum += w * h;
            projs.push(Proj {
                size: [w, h],
                n2: [nrm(p0), nrm(p1), nrm(p2)],
                pos: [a.into(), b.into(), c.into()],
                src_uv: [ua, ub, uc],
            });
        }

        // 2. Distribute a fixed texel budget by area → a density (texels per object unit),
        //    then shelf-pack. If the layout overflows the max texture, drop density and retry
        //    (graceful quality loss beats failing to paint at all).
        let budget = (tris as f32 * PX_PER_TRI * PX_PER_TRI).min((MAX_ATLAS_EDGE as f32 * 0.7).powi(2));
        let mut density = (budget / area_sum.max(1e-6)).sqrt();
        loop {
            let rects: Vec<[u32; 2]> = projs
                .iter()
                .map(|p| {
                    let rw = ((p.size[0] * density).round() as u32).clamp(1, MAX_ATLAS_EDGE - 2 * ATLAS_PAD);
                    let rh = ((p.size[1] * density).round() as u32).clamp(1, MAX_ATLAS_EDGE - 2 * ATLAS_PAD);
                    [rw + 2 * ATLAS_PAD, rh + 2 * ATLAS_PAD]
                })
                .collect();
            if let Some((edge, places)) = shelf_pack(&rects, MAX_ATLAS_EDGE) {
                let inv_edge = 1.0 / edge as f32;
                let mut verts = Vec::with_capacity(tris * 3);
                let mut indices = Vec::with_capacity(tris * 3);
                let mut cells = Vec::with_capacity(tris);
                let mut orig_vids = Vec::with_capacity(tris * 3);
                for t in 0..tris {
                    let p = &projs[t];
                    let [ox, oy] = places[t];
                    let [rw, rh] = rects[t];
                    // Inner box (inside the padding) the triangle's vertices map into.
                    let ix = (ox + ATLAS_PAD) as f32;
                    let iy = (oy + ATLAS_PAD) as f32;
                    let iw = (rw - 2 * ATLAS_PAD).max(1) as f32;
                    let ih = (rh - 2 * ATLAS_PAD).max(1) as f32;
                    let mut uv = [[0.0f32; 2]; 3];
                    for (k, slot) in uv.iter_mut().enumerate() {
                        *slot = [
                            (ix + p.n2[k][0] * iw) * inv_edge,
                            (iy + p.n2[k][1] * ih) * inv_edge,
                        ];
                    }
                    for (k, &vuv) in uv.iter().enumerate() {
                        let vid = pp.indices[t * 3 + k];
                        let mut v = pp.verts[vid as usize];
                        v.uv = vuv;
                        indices.push(verts.len() as u32);
                        verts.push(v);
                        orig_vids.push(vid);
                    }
                    cells.push(AtlasCell { uv, pos: p.pos, src_uv: p.src_uv });
                }
                // Carry imported COLOR_0 over to the unshared atlas vertices, so a painted
                // node keeps the mesh's Blender-painted look (registering a mesh with colors
                // allocates its paint block, exactly like the original mesh's registration).
                let colors = pp.colors.as_ref().map(|c| {
                    orig_vids
                        .iter()
                        .map(|&v| c.get(v as usize).copied().unwrap_or([255; 4]))
                        .collect()
                });
                return Some(MeshAtlas {
                    mesh: MeshData { vertices: verts, indices, colors },
                    edge,
                    cells,
                    orig_vids,
                });
            }
            density *= 0.8;
            if density < 1e-4 {
                return None; // geometry too pathological to pack — give up rather than hang
            }
        }
    }

    pub(crate) fn part_count(&self, key: &str) -> usize {
        self.parts.get(key).map_or(0, |p| p.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quad() -> MeshData {
        // Two triangles spanning x,y ∈ [-1,1] at z = 0, facing +Z; UV maps x,y ∈ [-1,1]
        // linearly to [0,1] (bottom-left = (0,0), top-right = (1,1)).
        let v = |x: f32, y: f32| Vertex {
            pos: [x, y, 0.0],
            normal: [0.0, 0.0, 1.0],
            uv: [(x + 1.0) * 0.5, (y + 1.0) * 0.5],
        };
        MeshData {
            vertices: vec![v(-1.0, -1.0), v(1.0, -1.0), v(1.0, 1.0), v(-1.0, 1.0)],
            indices: vec![0, 1, 2, 0, 2, 3],
            colors: None,
        }
    }

    #[test]
    fn ray_hits_a_quad_dead_on() {
        let p = PaintPart::build(&quad());
        let hit = p.raycast(Vec3::new(0.0, 0.0, 5.0), Vec3::new(0.0, 0.0, -1.0), 100.0);
        let h = hit.expect("ray down -Z must hit the quad");
        assert!((h.t - 5.0).abs() < 1e-3, "t = {}", h.t);
        assert!(h.pos.length() < 1e-3, "should hit the origin, got {:?}", h.pos);
        assert!(h.normal.z.abs() > 0.9, "normal should face ±Z, got {:?}", h.normal);
    }

    /// THE repeating fix: every triangle owns a UNIQUE, non-overlapping patch of the atlas,
    /// so a dab on one triangle can never land on another — even when the mesh's own UVs tile.
    #[test]
    fn atlas_gives_each_triangle_a_disjoint_uv_cell() {
        let mut cache = PaintMeshCache::default();
        cache.get_or_build("q", || vec![quad()]); // 2 triangles
        let atlas = cache.atlas_mesh("q", 0).expect("atlas");
        assert_eq!(atlas.mesh.vertices.len(), 6, "2 tris → 6 unshared verts");
        assert_eq!(atlas.cells.len(), 2, "one cell per triangle");
        // The render mesh's per-triangle UVs must equal the cell's stored UVs.
        for (t, cell) in atlas.cells.iter().enumerate() {
            for k in 0..3 {
                assert_eq!(atlas.mesh.vertices[t * 3 + k].uv, cell.uv[k], "mesh UV == cell UV");
            }
        }
        // The two triangles' texel footprints (their rasterized texel sets) must be DISJOINT
        // — that is what makes a dab on one impossible to see on the other.
        let footprint = |cell: &AtlasCell| {
            let mut set = std::collections::HashSet::new();
            for_each_cell_texel(cell, atlas.edge, |idx, _| {
                set.insert(idx);
            });
            set
        };
        let f0 = footprint(&atlas.cells[0]);
        let f1 = footprint(&atlas.cells[1]);
        assert!(!f0.is_empty() && !f1.is_empty(), "each cell must cover some texels");
        assert!(f0.is_disjoint(&f1), "the two triangles' texels must not overlap");
    }

    /// A painted node must keep its imported COLOR_0 look: the atlas duplicates vertices
    /// (three per triangle, unshared), so the color stream is remapped through the original
    /// index list — atlas vertex j carries the color of original vertex `orig_vids[j]`.
    #[test]
    fn atlas_carries_imported_vertex_colors_over() {
        let mut d = quad();
        // Distinct color per original vertex, so any remap slip is visible.
        d.colors = Some(vec![[10, 0, 0, 255], [0, 20, 0, 255], [0, 0, 30, 255], [40, 40, 40, 255]]);
        let src_colors = d.colors.clone().unwrap();
        let src_indices = d.indices.clone();
        let mut cache = PaintMeshCache::default();
        cache.get_or_build("c", || vec![d]);
        let atlas = cache.atlas_mesh("c", 0).expect("atlas");
        assert_eq!(atlas.orig_vids, src_indices, "atlas vertex j ↔ original index j");
        let colors = atlas.mesh.colors.as_ref().expect("colors carried over");
        assert_eq!(colors.len(), atlas.mesh.vertices.len(), "register() needs parallel streams");
        for (j, &vid) in atlas.orig_vids.iter().enumerate() {
            assert_eq!(colors[j], src_colors[vid as usize], "atlas vert {j} color");
        }
        // And a mesh WITHOUT colors must not fabricate any (register would alloc a block).
        let mut cache2 = PaintMeshCache::default();
        cache2.get_or_build("p", || vec![quad()]);
        assert!(cache2.atlas_mesh("p", 0).unwrap().mesh.colors.is_none());
    }

    /// Uniform texel density: a long, thin triangle is packed into a long, thin rect (not a
    /// square), so its texels stay roughly square in world space instead of stretching. The
    /// rect's aspect ratio must track the triangle's flattened aspect ratio.
    #[test]
    fn stretched_triangles_get_proportioned_cells() {
        // A 10×0.5 sliver quad — the shape that used to stretch under the uniform grid.
        let v = |x: f32, y: f32| Vertex { pos: [x, y, 0.0], normal: [0.0, 0.0, 1.0], uv: [0.0; 2] };
        let d = MeshData {
            vertices: vec![v(0.0, 0.0), v(10.0, 0.0), v(10.0, 0.5), v(0.0, 0.5)],
            indices: vec![0, 1, 2, 0, 2, 3],
            colors: None,
        };
        let mut cache = PaintMeshCache::default();
        cache.get_or_build("s", || vec![d]);
        let atlas = cache.atlas_mesh("s", 0).expect("atlas");
        // Each triangle's atlas-UV bounding box should be far wider than it is tall.
        for cell in &atlas.cells {
            let xs = cell.uv.iter().map(|u| u[0]);
            let ys = cell.uv.iter().map(|u| u[1]);
            let w = xs.clone().fold(f32::MIN, f32::max) - xs.fold(f32::MAX, f32::min);
            let h = ys.clone().fold(f32::MIN, f32::max) - ys.fold(f32::MAX, f32::min);
            assert!(w > h * 3.0, "sliver cell should be wide, got {w}×{h}");
        }
    }

    #[test]
    fn ray_misses_when_aimed_away() {
        let p = PaintPart::build(&quad());
        assert!(p.raycast(Vec3::new(0.0, 0.0, 5.0), Vec3::new(0.0, 0.0, 1.0), 100.0).is_none());
        assert!(p.raycast(Vec3::new(9.0, 9.0, 5.0), Vec3::new(0.0, 0.0, -1.0), 100.0).is_none());
    }

    #[test]
    fn ray_takes_the_nearest_of_two_surfaces() {
        // Two quads: one at z = 0, one at z = 2. A ray from z = 5 must strike the far
        // one FIRST (z = 2) — a nearest-hit bug would sail through to z = 0.
        let mut d = quad();
        let base = d.vertices.len() as u32;
        for i in 0..4 {
            let mut v = d.vertices[i];
            v.pos[2] = 2.0;
            d.vertices.push(v);
        }
        d.indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        let p = PaintPart::build(&d);
        let h = p
            .raycast(Vec3::new(0.0, 0.0, 5.0), Vec3::new(0.0, 0.0, -1.0), 100.0)
            .expect("must hit");
        assert!((h.t - 3.0).abs() < 1e-2, "should hit the NEAR quad at z=2 (t=3), got t={}", h.t);
        assert!((h.pos.z - 2.0).abs() < 1e-2, "got {:?}", h.pos);
    }

    /// A big triangle among fine ones — the RetroMap-shaped case. Before the oversized
    /// escape hatch, `cell` came out tiny (mean edge) while the big quad's AABB spanned
    /// (extent/cell)³ cells, and `build` would try to register it into millions of them.
    /// The real thing: build + raycast every model in `assets/models/_test`, at the
    /// max_t the brush actually passes. This is the test that speaks to the reported
    /// freeze — synthetic geometry is where the original bug hid.
    #[test]
    fn real_models_build_and_raycast_fast() {
        let dir = std::path::Path::new("../../assets/models/_test");
        if !dir.is_dir() {
            return; // not a checkout with the test models — nothing to say
        }
        let mut tested = 0usize;
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("glb") {
                continue;
            }
            let Ok(model) = floptle_assets::import(&path) else { continue };
            for part in &model.parts {
                tested += 1;
                let t0 = std::time::Instant::now();
                let pp = PaintPart::build(&part.mesh);
                let build_ms = t0.elapsed().as_secs_f32() * 1000.0;
                assert!(
                    build_ms < 2000.0,
                    "{:?}: PaintPart::build took {build_ms:.0}ms",
                    path.file_name().unwrap()
                );
                // 60 casts ≈ a second of hovering. Misses AND hits, at the real max_t.
                let t0 = std::time::Instant::now();
                for i in 0..60 {
                    let a = i as f32 * 0.1;
                    pp.raycast(Vec3::new(a.cos() * 50.0, 50.0, a.sin() * 50.0), Vec3::new(0.0, -1.0, 0.0), 1e5);
                    pp.raycast(Vec3::new(9e3, 9e3, 9e3), Vec3::new(0.0, 0.0, -1.0), 1e5);
                }
                let cast_ms = t0.elapsed().as_secs_f32() * 1000.0;
                assert!(
                    cast_ms < 500.0,
                    "{:?}: 120 raycasts took {cast_ms:.0}ms — a frame's worth would freeze the editor",
                    path.file_name().unwrap()
                );
                println!(
                    "  {:24} part {tested}: build {build_ms:6.1}ms  120 casts {cast_ms:6.1}ms  ({} tris, {} oversized)",
                    path.file_name().unwrap().to_string_lossy(),
                    part.mesh.indices.len() / 3,
                    pp.oversized.len(),
                );
            }
        }
        // Guard the guard: a silently-skipped loop would make this test meaningless.
        assert!(tested > 0, "no models were actually tested — the path is wrong");
    }

    #[test]
    fn build_survives_mixed_triangle_sizes() {
        let v = |x: f32, y: f32, z: f32| Vertex { pos: [x, y, z], normal: [0.0, 0.0, 1.0], uv: [0.0; 2] };
        let mut d = MeshData::default();
        // One enormous floor quad.
        d.vertices.extend([v(-500.0, 0.0, -500.0), v(500.0, 0.0, -500.0), v(500.0, 0.0, 500.0)]);
        d.indices.extend([0, 1, 2]);
        // …plus a strip of very fine triangles, which drags the mean edge down.
        for i in 0..300u32 {
            let x = i as f32 * 0.001;
            let b = d.vertices.len() as u32;
            d.vertices.extend([v(x, 1.0, 0.0), v(x + 0.001, 1.0, 0.0), v(x, 1.001, 0.0)]);
            d.indices.extend([b, b + 1, b + 2]);
        }
        let p = PaintPart::build(&d); // must return, not hang or OOM
        assert!(!p.oversized.is_empty(), "the floor quad must be treated as oversized");
        assert!(p.grid.len() < 100_000, "grid blew up: {} cells", p.grid.len());
        // …and the oversized triangle must still be HITTABLE.
        let hit = p.raycast(Vec3::new(0.0, 5.0, 0.0), Vec3::new(0.0, -1.0, 0.0), 1e5);
        let h = hit.expect("the big floor quad must still be raycastable");
        assert!((h.t - 5.0).abs() < 1e-2, "t = {}", h.t);
    }

    /// The freeze that shipped: `raycast` marched t=0..max_t at half-cell steps, so a
    /// fine mesh + the real max_t (1e5) meant tens of millions of iterations per mesh
    /// per frame. A MISS is the common case (most of the scene isn't under the cursor),
    /// so it must be near-free.
    #[test]
    fn a_miss_on_a_fine_mesh_is_cheap() {
        let v = |x: f32, y: f32| Vertex { pos: [x, y, 0.0], normal: [0.0, 0.0, 1.0], uv: [0.0; 2] };
        let mut d = MeshData::default();
        for i in 0..2000u32 {
            let x = i as f32 * 0.002; // 2mm triangles → a tiny cell
            let b = d.vertices.len() as u32;
            d.vertices.extend([v(x, 0.0), v(x + 0.002, 0.0), v(x, 0.002)]);
            d.indices.extend([b, b + 1, b + 2]);
        }
        let p = PaintPart::build(&d);
        let t0 = std::time::Instant::now();
        for _ in 0..100 {
            // Aimed well away from the mesh, with the SAME max_t the brush passes.
            assert!(p.raycast(Vec3::new(0.0, 900.0, 5.0), Vec3::new(0.0, 0.0, -1.0), 1e5).is_none());
        }
        let ms = t0.elapsed().as_secs_f32() * 1000.0;
        assert!(ms < 50.0, "100 missing raycasts took {ms:.1}ms — the march is unbounded again");
    }

    /// The other freeze: a brush radius spanning many cells swept (2r+1)³ of them.
    #[test]
    fn a_wide_brush_on_a_fine_mesh_is_cheap() {
        let v = |x: f32| Vertex { pos: [x, 0.0, 0.0], normal: [0.0, 0.0, 1.0], uv: [0.0; 2] };
        let d = MeshData {
            vertices: (0..3000).map(|i| v(i as f32 * 0.001)).collect(),
            indices: vec![0, 1, 2],
            colors: None,
        };
        let p = PaintPart::build(&d);
        let t0 = std::time::Instant::now();
        for _ in 0..100 {
            // radius 0.5 against a ~1mm cell = r≈500 ⇒ 1001³ cells if swept naively.
            let n = p.in_radius(Vec3::ZERO, 0.5).len();
            assert!(n > 0 && n <= 3000);
        }
        let ms = t0.elapsed().as_secs_f32() * 1000.0;
        assert!(ms < 50.0, "100 wide-brush queries took {ms:.1}ms — the cell sweep is unbounded again");
    }

    #[test]
    fn radius_query_finds_only_vertices_inside_it() {
        let p = PaintPart::build(&quad());
        let near = p.in_radius(Vec3::new(-1.0, -1.0, 0.0), 0.5);
        assert_eq!(near.len(), 1, "only the corner at (-1,-1) is within 0.5");
        assert_eq!(near[0].0, 0);
        assert_eq!(p.in_radius(Vec3::ZERO, 5.0).len(), 4, "a wide brush covers every corner");
        assert!(p.in_radius(Vec3::new(50.0, 0.0, 0.0), 0.5).is_empty());
    }
}
