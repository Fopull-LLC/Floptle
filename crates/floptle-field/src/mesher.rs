//! Surface nets: turn a [`ChunkField`] chunk into triangles
//! (`docs/subsystems/deformable-matter.md` §3.2).
//!
//! # Why surface nets, and not the alternatives
//!
//! * **vs marching cubes** — MC emits 2-5× the triangles for the same field and carries
//!   a 256-entry table as permanent maintenance. Surface nets places one vertex per
//!   surface cell and joins them into quads: smooth, low-poly output that suits both
//!   sculpted-organic terrain and a retro triangle budget.
//! * **vs dual contouring** — DC's QEF solve buys *sharp feature* reconstruction.
//!   smin-blended sculpted terrain has no sharp features to reconstruct, so that is
//!   complexity with no payoff here. (Revisit only if hard-edged CSG stamps ship.)
//!
//! # The one choice that matters
//!
//! **Vertex normals come from the field gradient, not from the triangles.** Face normals
//! (or their averages) would reintroduce exactly the faceting this whole effort exists to
//! kill. `ChunkField::grad` samples the f32 field and the rasterizer interpolates the
//! result across each triangle — which is what makes terrain shade like every imported
//! mesh, and is why SSAO/lighting "just work" on it.
//!
//! # Seams
//!
//! Nothing here reads chunk-local arrays. Cells, corners, and gradients all address the
//! field by *global voxel index* through the border-transparent API, so a chunk's +face
//! cells see their neighbour's voxels and two adjacent chunks compute byte-identical
//! vertices and normals on their shared boundary (trap T3).

use floptle_core::math::Vec3;

use crate::chunks::{ChunkField, CHUNK};

/// One chunk's extracted geometry. Positions are chunk-local (small numbers); `origin`
/// places them in field space, which is what the per-chunk instance matrix carries —
/// keeping vertex coordinates tiny is what makes this floating-origin-safe.
#[derive(Clone, Debug, Default)]
pub struct ChunkMesh {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub colors: Vec<[u8; 4]>,
    /// Per vertex, when the field bakes them ([`ChunkField::set_slot_edge_reach`]):
    /// the signed distance in world units to the nearest ground of another
    /// material, and which two materials, as `lo + hi * 256`. Positive on the
    /// `hi` side, so the boundary is where it crosses zero, inside a triangle
    /// wherever it falls rather than along the mesh. `[0, 0]` (and an empty
    /// list when not baked) is no other material within reach.
    pub edges: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
    pub origin: [f32; 3],
    /// How many of the triangles, at the end of `indices`, are skirt curtains rather
    /// than surface.
    pub skirt_tris: usize,
}

impl ChunkMesh {
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty()
    }
    pub fn tri_count(&self) -> usize {
        self.indices.len() / 3
    }
}

/// The 12 edges of a cell, as (corner a, corner b) in the standard corner numbering
/// `bit0 = +x, bit1 = +y, bit2 = +z`.
const EDGES: [(usize, usize); 12] = [
    (0, 1), (2, 3), (4, 5), (6, 7), // along x
    (0, 2), (1, 3), (4, 6), (5, 7), // along y
    (0, 4), (1, 5), (2, 6), (3, 7), // along z
];

#[inline]
fn corner_offset(c: usize) -> [i32; 3] {
    [(c & 1) as i32, ((c >> 1) & 1) as i32, ((c >> 2) & 1) as i32]
}


/// The chunk's voxel neighbourhood, gathered once into flat scratch.
///
/// Sampling straight through `ChunkField` costs a HashMap lookup per voxel, and surface
/// nets needs ~48 per vertex for the gradient alone — that measured 11 ms/chunk, 11× over
/// the sculpting budget. Gathering the box once turns every inner-loop read into array
/// indexing. It reads the field through exactly the same global-voxel addressing, so
/// border transparency (and therefore seam agreement) is preserved (T3).
pub struct MeshScratch {
    lo: [i32; 3],
    dim: usize,
    dist: Vec<f32>,
    color: Vec<[u8; 4]>,
    voxel: f32,
    band: f32,
    /// [`ChunkField::slot_edge_reach`] when gathered: how far each vertex looks
    /// for ground of another material.
    edge_reach: u8,
    /// The chunk + stride this scratch was gathered for — `mesh_scratch` reads them
    /// back so a queued job is fully self-contained (the async remesh worker meshes
    /// from the scratch alone and never touches the live field; trap T4).
    chunk: [i32; 3],
    stride: i32,
    /// The first voxel meshed, and how many cells along each edge: a chunk's
    /// own, or the small box [`mesh_box`] asks for.
    base: [i32; 3],
    cells: i32,
    /// Which of the 27 chunks around this one hold data, bit `(z+1)*9 + (y+1)*3 + (x+1)`
    /// for neighbour offset `(x, y, z)`.
    meshed: u32,
}

impl MeshScratch {
    /// The neighbourhood of `cells`³ full-detail cells from voxel `base`, for
    /// [`mesh_box`].
    fn new_box(field: &ChunkField, base: [i32; 3], cells: i32) -> Self {
        // Full detail reads no further than this from its cells.
        let margin = mesh_depends(1);
        let lo = base.map(|v| v - margin);
        let dim = (cells + 2 * margin + 1) as usize;
        let mut dist = Vec::new();
        let mut color = Vec::new();
        field.gather(lo, dim, &mut dist, &mut color);
        // At full detail no corner takes a minimum, so which chunks hold data
        // is never asked.
        let chunk = base.map(|v| v.div_euclid(CHUNK));
        Self { lo, dim, dist, color, voxel: field.voxel(), band: field.band(), edge_reach: field.slot_edge_reach(), chunk, stride: 1, base, cells, meshed: 0 }
    }

    fn new(field: &ChunkField, chunk: [i32; 3], stride: i32) -> Self {
        // Cells reach one stride past the chunk on each side: the borrowed -1 layer below,
        // and at coarse strides the skirted overlap layer above. Each coarse corner is the
        // minimum over half a stride around it. A vertex then settles up to `reach`
        // voxels and checks the ground it lands on half a stride either side, and
        // gradients need ±1 voxel plus the trilinear cell around that.
        //
        // Every value a vertex in the -1 layer reads must be inside the box, or the
        // chunk below computes it differently from the chunk that owns the cell and
        // the seam opens.
        //
        // The margin must scale with stride. Fixed at 2, LOD strides ≥ 2 read their -1
        // layer from outside the gathered box, where `at` reports open air — coarse chunks
        // grew vertices tens of units off the surface (caught by
        // `lod_strides_shed_triangles_and_still_hold_the_surface`).
        let margin = mesh_reads(stride);
        let lo = [
            chunk[0] * CHUNK - margin,
            chunk[1] * CHUNK - margin,
            chunk[2] * CHUNK - margin,
        ];
        let dim = (CHUNK + 2 * margin + 1) as usize;
        let mut dist = Vec::new();
        let mut color = Vec::new();
        field.gather(lo, dim, &mut dist, &mut color);
        let mut meshed = 0u32;
        for (bit, o) in (0..27).map(|b| (b, [b % 3 - 1, b / 3 % 3 - 1, b / 9 - 1])) {
            if field.has_data([chunk[0] + o[0], chunk[1] + o[1], chunk[2] + o[2]]) {
                meshed |= 1 << bit;
            }
        }
        let base = chunk.map(|v| v * CHUNK);
        let cells = (CHUNK / stride).max(1);
        Self { lo, dim, dist, color, voxel: field.voxel(), band: field.band(), edge_reach: field.slot_edge_reach(), chunk, stride, base, cells, meshed }
    }

    /// Flat scratch index for a global voxel, without bounds checks.
    ///
    /// Sound by construction: every voxel a corner's minimum reads lies within
    /// `stride + stride/2` of the chunk, and the margin is larger than that. The corner
    /// pass is the hot path and the checks cost more than the work.
    #[inline]
    fn idx(&self, i: [i32; 3]) -> usize {
        let x = (i[0] - self.lo[0]) as usize;
        let y = (i[1] - self.lo[1]) as usize;
        let z = (i[2] - self.lo[2]) as usize;
        debug_assert!(x < self.dim && y < self.dim && z < self.dim, "scratch overrun at {i:?}");
        (z * self.dim + y) * self.dim + x
    }

    /// Corner sample on the fast path — the caller guarantees the index is in the box.
    #[inline]
    fn at_fast(&self, i: [i32; 3]) -> f32 {
        self.dist[self.idx(i)]
    }

    #[inline]
    fn at(&self, i: [i32; 3]) -> f32 {
        let x = i[0] - self.lo[0];
        let y = i[1] - self.lo[1];
        let z = i[2] - self.lo[2];
        // Outside the gathered box means outside the band anyway: open air.
        if x < 0 || y < 0 || z < 0 {
            return self.band;
        }
        let (x, y, z) = (x as usize, y as usize, z as usize);
        if x >= self.dim || y >= self.dim || z >= self.dim {
            return self.band;
        }
        self.dist[(z * self.dim + y) * self.dim + x]
    }

    #[inline]
    fn color_at(&self, i: [i32; 3]) -> [u8; 4] {
        let x = i[0] - self.lo[0];
        let y = i[1] - self.lo[1];
        let z = i[2] - self.lo[2];
        if x < 0 || y < 0 || z < 0 {
            return [128, 128, 128, 255];
        }
        let (x, y, z) = (x as usize, y as usize, z as usize);
        if x >= self.dim || y >= self.dim || z >= self.dim {
            return [128, 128, 128, 255];
        }
        self.color[(z * self.dim + y) * self.dim + x]
    }

    /// What [`ChunkMesh::edges`] holds for a vertex at `p` (field space) of
    /// material `own`: the signed distance to the nearest ground of another
    /// material within [`Self::edge_reach`] voxels, and the pair.
    ///
    /// Only ground counts: a voxel within three quarters of a voxel of the
    /// surface. A layer a voxel under undug ground is a voxel deep, so it is
    /// never found, and the surface over it never blends toward it. The
    /// boundary is taken halfway between the two voxels, so the distance is
    /// to that, not to the other voxel.
    fn slot_edge(&self, p: Vec3, own: u8) -> [f32; 2] {
        let r = self.edge_reach as i32;
        if r == 0 || own == 0 || own == 255 {
            return [0.0, 0.0];
        }
        let g = p / self.voxel;
        let c = [g.x.round() as i32, g.y.round() as i32, g.z.round() as i32];
        let ground = 0.75 * self.voxel;
        let (mut best, mut other) = (f32::MAX, 0u8);
        for dz in -r..=r {
            for dy in -r..=r {
                for dx in -r..=r {
                    let i = [c[0] + dx, c[1] + dy, c[2] + dz];
                    let (x, y, z) = (i[0] - self.lo[0], i[1] - self.lo[1], i[2] - self.lo[2]);
                    if x < 0 || y < 0 || z < 0 || x as usize >= self.dim || y as usize >= self.dim || z as usize >= self.dim {
                        continue;
                    }
                    let k = (z as usize * self.dim + y as usize) * self.dim + x as usize;
                    let slot = self.color[k][3];
                    if slot == own || slot == 0 || slot == 255 || self.dist[k].abs() > ground {
                        continue;
                    }
                    let at = Vec3::new(i[0] as f32, i[1] as f32, i[2] as f32) * self.voxel;
                    let d = (at - p).length();
                    if d < best {
                        best = d;
                        other = slot;
                    }
                }
            }
        }
        if other == 0 {
            return [0.0, 0.0];
        }
        let to_edge = (best - 0.5 * self.voxel).max(0.0);
        let (lo, hi) = (own.min(other), own.max(other));
        [if own == hi { to_edge } else { -to_edge }, lo as f32 + hi as f32 * 256.0]
    }

    /// Trilinear distance at a world position — identical maths to `ChunkField::d`.
    fn d(&self, p: Vec3) -> f32 {
        let g = p / self.voxel;
        let b = [g.x.floor() as i32, g.y.floor() as i32, g.z.floor() as i32];
        let f = Vec3::new(g.x - b[0] as f32, g.y - b[1] as f32, g.z - b[2] as f32);
        let c = |dx: i32, dy: i32, dz: i32| self.at([b[0] + dx, b[1] + dy, b[2] + dz]);
        let l = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let x00 = l(c(0, 0, 0), c(1, 0, 0), f.x);
        let x10 = l(c(0, 1, 0), c(1, 1, 0), f.x);
        let x01 = l(c(0, 0, 1), c(1, 0, 1), f.x);
        let x11 = l(c(0, 1, 1), c(1, 1, 1), f.x);
        l(l(x00, x10, f.y), l(x01, x11, f.y), f.z)
    }

    /// The vertex normal: the field's gradient, in f32, once per vertex — not a face
    /// normal. This is the line that retires the up-close faceting.
    fn grad(&self, p: Vec3) -> Vec3 {
        let h = self.voxel;
        Vec3::new(
            self.d(p + Vec3::X * h) - self.d(p - Vec3::X * h),
            self.d(p + Vec3::Y * h) - self.d(p - Vec3::Y * h),
            self.d(p + Vec3::Z * h) - self.d(p - Vec3::Z * h),
        )
        .normalize_or_zero()
    }
}

/// How far, in voxels, a coarse vertex looks for the full-resolution surface, and how far
/// a skirt can drop into the solid. One coarse cell covers how far the solid-keeping
/// corners can push a surface outward; four voxels at full detail covers the sag of a
/// coarser neighbour's surface under this one.
fn reach(stride: i32) -> i32 {
    stride.max(4)
}

/// How far past its own chunk, in voxels, the scratch a chunk's mesh at `stride`
/// is built from reaches: every voxel the mesher may read.
pub fn mesh_reads(stride: i32) -> i32 {
    let stride = stride.max(1);
    stride + reach(stride) + stride / 2 + 3
}

/// How far past its own chunk, in voxels, a change to the field can alter a
/// chunk's unskirted mesh at `stride`. A collider that keeps meshes needs only
/// re-mesh the chunks within this of an edit.
///
/// Tighter than [`mesh_reads`] at full detail, where no vertex settles: corners
/// reach one voxel past the chunk and a normal reads a voxel either side of its
/// vertex, through the trilinear cell around that, so four voxels hold every
/// value the mesh is built from.
pub fn mesh_depends(stride: i32) -> i32 {
    if stride <= 1 { 4 } else { mesh_reads(stride) }
}

/// Each face of a cell as its four corners in order around the face, and the cell edge
/// joining each corner to the next. Opposite faces list matching corners in the same
/// order, so the two cells sharing a face add its corners up in the same order and
/// always agree on how it splits.
const FACES: [([usize; 4], [usize; 4]); 6] = [
    ([0, 2, 6, 4], [4, 10, 6, 8]),  // -x
    ([1, 3, 7, 5], [5, 11, 7, 9]),  // +x
    ([0, 1, 5, 4], [0, 9, 2, 8]),   // -y
    ([2, 3, 7, 6], [1, 11, 3, 10]), // +y
    ([0, 1, 3, 2], [0, 5, 1, 4]),   // -z
    ([4, 5, 7, 6], [2, 7, 3, 6]),   // +z
];

/// Groups of corners on one side of the surface, joined along the cube's edges.
const fn corner_groups(mask: u8, solid: bool) -> u32 {
    let mut parent = [0usize, 1, 2, 3, 4, 5, 6, 7];
    let mut e = 0;
    while e < 12 {
        let (a, b) = EDGES[e];
        if (((mask >> a) & 1) == 1) == solid && (((mask >> b) & 1) == 1) == solid {
            let mut ra = a;
            while parent[ra] != ra {
                ra = parent[ra];
            }
            let mut rb = b;
            while parent[rb] != rb {
                rb = parent[rb];
            }
            parent[ra] = rb;
        }
        e += 1;
    }
    let mut n = 0;
    let mut k = 0;
    while k < 8 {
        if (((mask >> k) & 1) == 1) == solid && parent[k] == k {
            n += 1;
        }
        k += 1;
    }
    n
}

/// Cells whose solid corners form one group and whose air corners form another hold a
/// single sheet of surface. Only the rest pay for [`split_sheets`]. A face whose corners
/// alternate solid/air can only occur in a cell with a split group, so the two cells
/// sharing such a face both take the slow path and decide it the same way.
static SINGLE_SHEET: [bool; 256] = {
    let mut t = [false; 256];
    let mut m = 1;
    while m < 255 {
        t[m] = corner_groups(m as u8, true) == 1 && corner_groups(m as u8, false) == 1;
        m += 1;
    }
    t
};

/// A sheet that runs across one face of its cell twice, cut into two vertices.
///
/// One vertex per sheet joins the sheet to the cell across that face by a single mesh
/// edge, and when that cell's sheet also runs across the face twice, all four quads
/// along the face share that edge — a pinch no rendering or welding can untangle.
/// Two vertices give each crossing its own edge; a fill triangle at each cut closes
/// the slit between them.
#[derive(Clone, Copy)]
struct Cut {
    /// Where the sheet was cut: each a pair of its crossing edges joined across one
    /// face of the cell, one edge on each half.
    at: [(usize, usize); 2],
}

/// A cell's sheets: how many, each crossing edge's sheet (two bits per edge), and the
/// sheet cut in two, if any.
struct Sheets {
    count: u32,
    map: u32,
    cut: Option<Cut>,
}

/// Split a cell's surface into separate sheets: crossing edges that share a face are the
/// same sheet where the surface runs across that face between them. A face crossed on
/// all four edges is joined through its centre when the centre is solid and split there
/// when it is air.
///
/// Every crossing edge lies on two faces and is joined to one other crossing edge on
/// each, so each sheet is a ring of crossing edges.
fn split_sheets(d: &[f32; 8], mask: u8) -> Sheets {
    let crosses = |e: usize| {
        let (a, b) = EDGES[e];
        ((mask >> a) & 1) != ((mask >> b) & 1)
    };
    let mut ring = [[usize::MAX; 2]; 12];
    let mut join = |a: usize, b: usize| {
        for (x, y) in [(a, b), (b, a)] {
            let slot = if ring[x][0] == usize::MAX { 0 } else { 1 };
            ring[x][slot] = y;
        }
    };
    // Faces crossed on all four edges, as their two segments.
    let mut twice = [((0usize, 0usize), (0usize, 0usize)); 6];
    let mut n_twice = 0;
    for (corners, edges) in FACES {
        let crossing = edges.iter().filter(|e| crosses(**e)).count();
        if crossing == 2 {
            let mut it = edges.iter().copied().filter(|e| crosses(*e));
            let (a, b) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
            join(a, b);
        } else if crossing == 4 {
            let centre = d[corners[0]] + d[corners[1]] + d[corners[2]] + d[corners[3]];
            let solid_joined = centre < 0.0;
            let mut segs = [(0usize, 0usize); 2];
            let mut n = 0;
            for k in 0..4 {
                // A corner on the side the centre cuts off gets its own short segment,
                // joining the two edges that meet at it.
                if (((mask >> corners[k]) & 1) == 1) != solid_joined {
                    let seg = (edges[(k + 3) % 4], edges[k]);
                    join(seg.0, seg.1);
                    segs[n.min(1)] = seg;
                    n += 1;
                }
            }
            twice[n_twice] = (segs[0], segs[1]);
            n_twice += 1;
        }
    }

    let mut sheet_of = [u32::MAX; 12];
    let mut count = 0u32;
    let mut cut = None;
    // A cube holds at most four sheets; anything past that shares the last.
    let next_id = |count: u32| count.min(3);
    for start in 0..12 {
        if !crosses(start) || sheet_of[start] != u32::MAX {
            continue;
        }
        let id = next_id(count);
        count += 1;
        // Walk the ring.
        let mut seq = [0usize; 12];
        let mut len = 0;
        let (mut prev, mut cur) = (usize::MAX, start);
        while cur != usize::MAX && sheet_of[cur] == u32::MAX && len < 12 {
            sheet_of[cur] = id;
            seq[len] = cur;
            len += 1;
            let next = if ring[cur][0] != prev { ring[cur][0] } else { ring[cur][1] };
            prev = cur;
            cur = next;
        }
        if cut.is_some() {
            continue;
        }
        let pos = |e: usize| seq[..len].iter().position(|x| *x == e);
        // Each face this ring runs across twice, as the ring positions its two segments
        // start at (a segment fills that position and the next).
        let mut units = [(0usize, 0usize); 12];
        let mut n_units = 0;
        for (face, &(s1, s2)) in twice[..n_twice].iter().enumerate() {
            for (a, b) in [s1, s2] {
                let (Some(pa), Some(pb)) = (pos(a), pos(b)) else { continue };
                units[n_units] = (if (pa + 1) % len == pb { pa } else { pb }, face);
                n_units += 1;
            }
        }
        let units = &mut units[..n_units];
        units.sort_unstable();
        let units = &*units;
        let twice_here = |f: usize| units.iter().filter(|u| u.1 == f).count() == 2;
        if !units.iter().any(|u| twice_here(u.1)) {
            continue;
        }
        // Cut in the stretches between segments: two cuts make two arcs, and every face
        // crossed twice must have one segment on each.
        let k = units.len();
        let gap = |g: usize| {
            let from = units[g].0 + 1;
            let to = units[(g + 1) % k].0 + if g + 1 == k { len } else { 0 };
            (to > from).then_some((from + to) / 2 % len)
        };
        // The pair of cuts that separates the most of them; normally all.
        let mut best = None;
        let mut best_score = 0;
        for g1 in 0..k {
            for g2 in g1 + 1..k {
                let (Some(c1), Some(c2)) = (gap(g1), gap(g2)) else { continue };
                let side = |i: usize| i > g1 && i <= g2;
                let score = (0..k)
                    .filter(|&i| {
                        twice_here(units[i].1)
                            && (0..k).any(|j| j != i && units[j].1 == units[i].1 && side(j) != side(i))
                    })
                    .count();
                if score > best_score {
                    best_score = score;
                    best = Some((c1, c2));
                }
            }
        }
        let Some((c1, c2)) = best else { continue };
        let other = next_id(count);
        count += 1;
        let mut p = (c1 + 1) % len;
        loop {
            sheet_of[seq[p]] = other;
            if p == c2 {
                break;
            }
            p = (p + 1) % len;
        }
        cut = Some(Cut { at: [(seq[c1], seq[(c1 + 1) % len]), (seq[c2], seq[(c2 + 1) % len])] });
    }
    let mut map = 0u32;
    for (e, s) in sheet_of.iter().enumerate() {
        if *s != u32::MAX {
            map |= s << (2 * e);
        }
    }
    Sheets { count: count.min(4), map, cut }
}

/// The edge of the neighbouring cell across face `axis` that is the same lattice edge
/// as this cell's edge `e`.
fn mirror_edge(e: usize, axis: usize) -> usize {
    let bit = 1 << axis;
    let (a, b) = EDGES[e];
    EDGES.iter().position(|&(x, y)| x == a ^ bit && y == b ^ bit).unwrap_or(e)
}

/// The direction the field rises in a cell at cell-local `l` (0..1 per axis), from its
/// eight corner values: the trilinear interpolant's gradient.
fn cell_gradient(d: &[f32; 8], l: Vec3) -> Vec3 {
    let (x, y, z) = (l.x, l.y, l.z);
    let (ix, iy, iz) = (1.0 - x, 1.0 - y, 1.0 - z);
    Vec3::new(
        iy * iz * (d[1] - d[0]) + y * iz * (d[3] - d[2]) + iy * z * (d[5] - d[4]) + y * z * (d[7] - d[6]),
        ix * iz * (d[2] - d[0]) + x * iz * (d[3] - d[1]) + ix * z * (d[6] - d[4]) + x * z * (d[7] - d[5]),
        ix * iy * (d[4] - d[0]) + x * iy * (d[5] - d[1]) + ix * y * (d[6] - d[2]) + x * y * (d[7] - d[3]),
    )
    .normalize_or_zero()
}

impl MeshScratch {
    /// Whether the chunk holding global voxel `i` is one that gets meshed. Chunks beyond
    /// the 27 this scratch knows about count as not meshed.
    fn meshed_at(&self, i: [i32; 3]) -> bool {
        let o = [
            i[0].div_euclid(CHUNK) - self.chunk[0],
            i[1].div_euclid(CHUNK) - self.chunk[1],
            i[2].div_euclid(CHUNK) - self.chunk[2],
        ];
        if o.iter().any(|v| v.abs() > 1) {
            return false;
        }
        self.meshed >> ((o[2] + 1) * 9 + (o[1] + 1) * 3 + (o[0] + 1)) & 1 == 1
    }

    /// The lowest value in the cube of `half` voxels around a corner. A coarse corner
    /// reads solid if any voxel it stands for is solid, so rock thinner than a coarse
    /// cell still has a solid corner to be meshed from.
    fn min_around(&self, g: [i32; 3], half: i32) -> f32 {
        // Stored values stop at -band, so a corner already there is the minimum.
        let own = self.at_fast(g);
        if own <= -self.band {
            return own;
        }
        let w = (2 * half + 1) as usize;
        let mut m = f32::MAX;
        for z in g[2] - half..=g[2] + half {
            for y in g[1] - half..=g[1] + half {
                let row = self.idx([g[0] - half, y, z]);
                for v in &self.dist[row..row + w] {
                    m = m.min(*v);
                }
            }
        }
        m
    }

    /// Where the full-resolution surface lies behind a coarse vertex: the first point,
    /// looking from `p` back along `-dir` up to `reach`, where the line enters the solid —
    /// or `p` itself if it finds none, or finds one facing another way.
    ///
    /// Coarse corners keep solid by taking the minimum around them, which pushes the
    /// surface they describe outward by up to most of a cell. Settling each vertex back
    /// onto the real surface keeps the coarse mesh where the ground is. Both chunks
    /// sharing a vertex settle it from the same values, so seams still agree.
    ///
    /// Only inward, and only onto ground that faces within 60° of the way the
    /// coarse surface does, judged at the coarse cell's scale or at one voxel's. A vertex that lands on the far wall of a cave
    /// or the side of a boulder while its neighbours land on the ground folds the
    /// triangles between them; staying put leaves it a little proud of the ground
    /// instead, which nobody sees at the distance a coarse chunk is drawn.
    fn settle(&self, p: Vec3, dir: Vec3, reach: f32) -> Vec3 {
        let step = self.voxel * 0.5;
        let mut prev = self.d(p);
        if prev < 0.0 {
            return p;
        }
        // Strides of half the distance the field reports, never under half a voxel:
        // the field rises no faster than distance, so a stride that size cannot step
        // over the ground, and the long reach a deep hollow needs costs a few samples.
        let mut t_prev = 0.0f32;
        while t_prev < reach {
            let t = (t_prev + (prev * 0.5).max(step)).min(reach);
            let dk = self.d(p - dir * t);
            if dk >= 0.0 {
                prev = dk;
                t_prev = t;
                continue;
            }
            // Half a voxel is short enough for the field to be nearly straight along it:
            // two rounds of interpolating between the bracketing samples land within a
            // hair of the crossing.
            let (mut a, mut da, mut b, mut db) = (t_prev, prev, t, dk);
            for _ in 0..2 {
                let m = a + (b - a) * (da / (da - db)).clamp(0.05, 0.95);
                let dm = self.d(p - dir * m);
                if dm < 0.0 {
                    (b, db) = (m, dm);
                } else {
                    (a, da) = (m, dm);
                }
            }
            let q = p - dir * (a + (b - a) * (da / (da - db)).clamp(0.0, 1.0));
            let facing = |h: f32| {
                Vec3::new(
                    self.d(q + Vec3::X * h) - self.d(q - Vec3::X * h),
                    self.d(q + Vec3::Y * h) - self.d(q - Vec3::Y * h),
                    self.d(q + Vec3::Z * h) - self.d(q - Vec3::Z * h),
                )
                .normalize_or_zero()
            };
            // The ground's facing at the coarse cell's scale, or at one voxel. Over
            // a crust thinner than half a stride the wide samples reach the void
            // under it and point anywhere, while one voxel either side still reads
            // the ground itself; either agreeing is ground the coarse surface can
            // stand on.
            let h = (self.stride / 2).max(1) as f32 * self.voxel;
            // Ground found further down than a cell has to face the coarse surface's
            // way more closely: a vertex dropped that far that lands on a slope its
            // neighbours did not reach folds the triangles between them.
            let far = (q - p).length() > (self.stride as f32 + 0.5) * self.voxel;
            let agrees = |n: Vec3| n.dot(dir) >= if far { 0.7 } else { 0.5 };
            return if agrees(facing(h)) || agrees(facing(self.voxel)) { q } else { p };
        }
        p
    }

    /// How far below a surface point, along `-n`, the skirt can drop and stay a little
    /// inside the solid: down through any air first (a coarse vertex can sit a little
    /// proud of the ground), then on through the solid up to `reach`. A thin roof gets a
    /// short skirt rather than one that pokes through into the cave under it. Never
    /// zero, so a skirt always has area.
    fn depth_below(&self, p: Vec3, n: Vec3, reach: f32) -> f32 {
        let step = self.voxel * 0.5;
        let inside = -0.25 * self.voxel;
        let mut depth = step * 0.5;
        let mut entered = None;
        let mut t = step;
        loop {
            let solid = self.d(p - n * t) < inside;
            match entered {
                None if solid => entered = Some(t),
                None if t > reach => break,
                Some(t0) if !solid || t > t0 + reach => break,
                _ => {}
            }
            if solid {
                depth = t;
            }
            t += step;
        }
        depth
    }
}

/// Mesh one chunk at `stride` (1 = full detail; 2^ℓ for LOD ℓ — the field resamples at
/// any stride, so LOD costs no extra storage).
///
/// # How far a coarse stride is from the field
///
/// Measured down radial rays at the drawn mesh against the field it came
/// from, on ground with 15 to 35 m of relief (drawn above the field is the
/// sign that matters: a coarse chunk is never the one a player stands on):
///
/// | Stride | Cell, 1.5 m voxel | p99 | Worst |
/// |---|---|---|---|
/// | 2 | 3 m | 0.5–0.65 m | 1.2–1.3 m |
/// | 4 | 6 m | 1.1–1.45 m | 3.0–4.1 m |
/// | 8 | 12 m | 2.8–4.1 m | 7.4–12.2 m |
///
/// Most of it is the triangles chording across bumps narrower than a cell,
/// which no placement of the vertices removes. The worst cases are pits
/// narrower than two cells, which the solid-keeping minimum fills: a crater
/// 25 m across and 15 m deep is drawn half full at stride 8. Dug ground is
/// meshed at full detail at any distance for that reason (the terrain
/// streamer's rule, not this function's).
///
/// A coarse corner is the minimum of the voxels it stands for, not a point sample, so
/// rock thinner than a coarse cell stays solid instead of falling between samples and
/// opening a view into the cave behind it. Each coarse vertex then settles back onto the
/// full-resolution surface.
///
/// `skirt` closes the seam against a neighbour meshed at another stride. Each chunk
/// builds its −x/−y/−z border from the neighbour's cells at its own stride, so a coarse
/// chunk ends half a coarse cell short of a finer neighbour on its + faces, which leaves
/// a slit you can see through from above. A skirted coarse chunk meshes one more cell
/// past its + faces, so the two meshes overlap instead. Every edge where the chunk's mesh
/// ends also gets a curtain dropped along −normal into the solid, which blocks the
/// grazing view under whichever overlapping mesh sits higher. Both sides of a stride
/// change need their curtains, so full-detail chunks drawn next to coarse ones are
/// skirted too.
pub fn mesh_chunk(field: &ChunkField, chunk: [i32; 3], stride: i32, skirt: bool) -> ChunkMesh {
    mesh_scratch(&scratch_for_chunk(field, chunk, stride), skirt)
}

/// The full-detail surface in the box of `cells`³ cells from voxel `base`: the
/// same triangles the chunks it lies in mesh there, for a caller that wants a
/// few cells of the drawn surface and not the cost of a chunk of it. Edges
/// whose low corner is in the box are meshed, as a chunk meshes its own.
pub fn mesh_box(field: &ChunkField, base: [i32; 3], cells: i32) -> ChunkMesh {
    mesh_scratch(&MeshScratch::new_box(field, base, cells.max(1)), false)
}

/// Gather everything `mesh_scratch` needs for one chunk into a self-contained scratch —
/// the cheap part (~0.07 ms bulk copy), done on the thread that owns the field. The
/// returned scratch can be shipped to a worker thread and meshed there without ever
/// touching the field again (the async remesh pipeline's contract, trap T4).
pub fn scratch_for_chunk(field: &ChunkField, chunk: [i32; 3], stride: i32) -> MeshScratch {
    MeshScratch::new(field, chunk, stride.max(1))
}

/// The heavy half of [`mesh_chunk`] (surface nets + per-vertex gradients, ~1-2 ms):
/// meshes entirely from the scratch, safe on any thread.
pub fn mesh_scratch(s: &MeshScratch, skirt: bool) -> ChunkMesh {
    let stride = s.stride;
    let voxel = s.voxel;
    let base = s.base;
    let origin = Vec3::new(base[0] as f32 * voxel, base[1] as f32 * voxel, base[2] as f32 * voxel);
    // Cells across the chunk at this stride. We mesh cells whose min-corner is inside
    // the chunk; their max corner reaches 1 stride into the neighbour, which the
    // border-transparent sampler serves — that overlap is what makes seams agree.
    let n = s.cells;
    // Cell indices run -1 ..= hi-1 on every axis, stored offset by +1.
    //
    // The extra negative layer is not an optimisation, it is what closes the mesh. A quad
    // for an edge on this chunk's -x/-y/-z face needs the four cells around that edge,
    // and two of them live in the neighbour. Without them each chunk could only emit its
    // strictly-interior edges, so every chunk boundary was a one-cell-wide hole — 336 of
    // a sphere's 3120 edges (see `the_assembled_field_mesh_has_no_holes`). Rasterized,
    // those holes showed the solid's inside face; the raymarch had never revealed them
    // because it hit the field, not the triangles.
    //
    // Each chunk emits exactly the edges whose min corner is its own voxel, so every edge
    // in the field is emitted once and only once — no duplicate triangles at seams. The
    // -1 layer's vertices duplicate the neighbour's, which is free: border-transparent
    // sampling makes them bit-identical (T3), so they weld invisibly.
    //
    // A skirted coarse chunk also takes the layer past its + faces (see `mesh_chunk`).
    // Against a neighbour at the same stride that layer repeats the neighbour's own
    // triangles exactly, which draws nothing new.
    let overlap = skirt && stride > 1;
    let hi = if overlap { n + 1 } else { n };
    let grid = (hi + 1) as usize;

    // Corner values, -1 ..= hi on every axis. At full detail a corner is its voxel; at a
    // coarse stride it is the lowest voxel within half a stride, so solid is never lost
    // between samples.
    //
    // Only where every edge touching the corner belongs to a meshed chunk, though. The
    // minimum reaches half a stride, past the narrow band, so it can make a corner solid
    // inside a chunk that holds no data; an edge such a chunk owns would then cross the
    // surface with nobody to mesh it, and leave a hole. Rock that thin right next to an
    // empty chunk is within the band of it, so that chunk would hold data.
    let gc = (hi + 2) as usize;
    let corner_idx = |x: i32, y: i32, z: i32| {
        (((z + 1) as usize * gc) + (y + 1) as usize) * gc + (x + 1) as usize
    };
    let half = stride / 2;
    let mut cv = vec![0.0f32; gc * gc * gc];
    for z in -1..=hi {
        for y in -1..=hi {
            for x in -1..=hi {
                let g = [base[0] + x * stride, base[1] + y * stride, base[2] + z * stride];
                let keep_solid = half > 0
                    && s.meshed_at(g)
                    && s.meshed_at([g[0] - stride, g[1], g[2]])
                    && s.meshed_at([g[0], g[1] - stride, g[2]])
                    && s.meshed_at([g[0], g[1], g[2] - stride]);
                cv[corner_idx(x, y, z)] =
                    if keep_solid { s.min_around(g, half) } else { s.at_fast(g) };
            }
        }
    }

    // vert_at[cell] -> the cell's first vertex, or u32::MAX. A cell holding more than one
    // sheet of surface has one vertex per sheet, numbered on from the first;
    // sheet_of[cell] says which sheet each of its edges belongs to, two bits per edge.
    let mut vert_at = vec![u32::MAX; grid * grid * grid];
    let mut sheet_of = vec![0u32; grid * grid * grid];
    // Cells whose sheet was cut in two, and where.
    let mut cuts: Vec<([i32; 3], Cut)> = Vec::new();
    let mut m = ChunkMesh { origin: origin.to_array(), ..Default::default() };
    let mut rim: Vec<bool> = Vec::new();
    // Per vertex: where it stood before settling, and the cell's facing, when
    // settling moved it.
    let mut unsettle: Vec<Option<(Vec3, Vec3)>> = Vec::new();

    let cell_idx = |x: i32, y: i32, z: i32| {
        (((z + 1) as usize * grid) + (y + 1) as usize) * grid + (x + 1) as usize
    };
    // The minimum over half a stride moves a flat surface with unit normal n outward by
    // half·|n|₁ voxels, at most half·√3, and fills a hollow narrower than a cell: a
    // vertex over a 30 m bowl at stride 8 stood 20 m over its floor. Settling looks
    // twice a cell down, so it reaches the floor of any hollow the minimum filled.
    let settle_reach = (4 * half) as f32 * voxel + voxel;

    // ---- pass 1: one vertex per sheet of surface in each cell, at the mean of that
    // sheet's edge crossings.
    //
    // One vertex for the whole cell, as plain surface nets has it, pinches together
    // sheets that only share the cell — a cave roof and the ground above it, both sides
    // of a thin ridge — and the quads around that vertex fold through each other.
    for cz in -1..hi {
        for cy in -1..hi {
            for cx in -1..hi {
                let mut d = [0.0f32; 8];
                let mut mask = 0u8;
                for (k, dk) in d.iter_mut().enumerate() {
                    let o = corner_offset(k);
                    *dk = cv[corner_idx(cx + o[0], cy + o[1], cz + o[2])];
                    if *dk < 0.0 {
                        mask |= 1 << k;
                    }
                }
                // All-in or all-out: no surface crosses this cell.
                if mask == 0 || mask == 0xFF {
                    continue;
                }
                let sheets = if SINGLE_SHEET[mask as usize] {
                    Sheets { count: 1, map: 0, cut: None }
                } else {
                    split_sheets(&d, mask)
                };
                let map = sheets.map;
                let c0 = [
                    base[0] + cx * stride,
                    base[1] + cy * stride,
                    base[2] + cz * stride,
                ];
                let first = m.positions.len() as u32;
                let outer = [cx, cy, cz].iter().any(|v| *v == -1 || *v == hi - 1);
                for sheet in 0..sheets.count {
                    let mut acc = Vec3::ZERO;
                    let mut count = 0.0f32;
                    for (e, &(a, b)) in EDGES.iter().enumerate() {
                        let (da, db) = (d[a], d[b]);
                        if (da < 0.0) == (db < 0.0) || (map >> (2 * e)) & 3 != sheet {
                            continue;
                        }
                        // Where the isosurface crosses this edge, linearly — kept a hair
                        // inside the edge. A corner a hair off zero (a one-voxel pocket the
                        // surface barely reaches) otherwise puts every surrounding cell's
                        // vertex on the corner itself, and those vertices weld into a pinch.
                        let t = if (db - da).abs() < 1e-12 { 0.5 } else { (-da / (db - da)).clamp(0.01, 0.99) };
                        let oa = corner_offset(a);
                        let ob = corner_offset(b);
                        let pa = Vec3::new(oa[0] as f32, oa[1] as f32, oa[2] as f32);
                        let pb = Vec3::new(ob[0] as f32, ob[1] as f32, ob[2] as f32);
                        acc += pa + (pb - pa) * t;
                        count += 1.0;
                    }
                    // Cell-local (0..1 per axis) -> chunk-local world units.
                    let local = acc / count.max(1.0);
                    let mut pos_field = Vec3::new(
                        (c0[0] as f32 + local.x * stride as f32) * voxel,
                        (c0[1] as f32 + local.y * stride as f32) * voxel,
                        (c0[2] as f32 + local.z * stride as f32) * voxel,
                    );
                    // The cell's own gradient: the direction the surface this vertex
                    // stands for faces.
                    let cell_dir = cell_gradient(&d, local);
                    let raw = pos_field;
                    if stride > 1 && cell_dir != Vec3::ZERO {
                        pos_field = s.settle(pos_field, cell_dir, settle_reach);
                    }
                    let nrm = vertex_normal(s, pos_field, cell_dir);
                    let col = s.color_at([
                        (pos_field.x / voxel).round() as i32,
                        (pos_field.y / voxel).round() as i32,
                        (pos_field.z / voxel).round() as i32,
                    ]);
                    m.positions.push((pos_field - origin).to_array());
                    rim.push(outer);
                    unsettle.push((pos_field != raw).then_some((raw, cell_dir)));
                    m.normals.push(nrm);
                    m.colors.push(col);
                    if s.edge_reach > 0 {
                        m.edges.push(s.slot_edge(pos_field, col[3]));
                    }
                }
                let c = cell_idx(cx, cy, cz);
                vert_at[c] = first;
                sheet_of[c] = map;
                if let Some(cut) = sheets.cut {
                    cuts.push(([cx, cy, cz], cut));
                }
            }
        }
    }

    if m.positions.is_empty() {
        return m;
    }

    // The vertex of the sheet that cell edge `e` belongs to.
    let vert = |x: i32, y: i32, z: i32, e: u32| {
        let c = cell_idx(x, y, z);
        match vert_at[c] {
            u32::MAX => u32::MAX,
            first => first + ((sheet_of[c] >> (2 * e)) & 3),
        }
    };

    // ---- pass 2: quads. For each axis edge at a cell's min corner, if the field
    // changes sign across it, the 4 cells around that edge each own a vertex — join
    // them. Winding follows the sign direction so faces point out of solid: CCW seen
    // from outside, which is what `front_face: Ccw` + `@builtin(front_facing)` read.
    // Asserted by `triangles_wind_outward` — this was inverted for both orders until the
    // P2 render swap made a consumer of it and the whole terrain rendered inside-out.
    //
    // Each quad splits along whichever diagonal leaves both triangles facing the way the
    // field does. A quad's corners are rarely coplanar, and splitting one along the
    // wrong diagonal folds it into a dark shard.
    let quad = |q: [u32; 4], flip: bool, m: &mut ChunkMesh| {
        if q.contains(&u32::MAX) {
            return;
        }
        let [a, b, c, d] = if flip { q } else { [q[0], q[3], q[2], q[1]] };
        let ac = facing(m, a, b, c).min(facing(m, a, c, d));
        let bd = facing(m, a, b, d).min(facing(m, b, c, d));
        if bd > ac {
            m.indices.extend_from_slice(&[a, b, d, b, c, d]);
        } else {
            m.indices.extend_from_slice(&[a, b, c, a, c, d]);
        }
    };
    let build = |m: &mut ChunkMesh| {
    // Every edge whose MIN corner is this chunk's own voxel — that ownership rule is
    // what makes the global cover exact (each edge emitted by exactly one chunk).
    for cz in 0..hi {
        for cy in 0..hi {
            for cx in 0..hi {
                let d0 = cv[corner_idx(cx, cy, cz)] < 0.0;
                // x edge -> quad in the y/z plane
                if (cv[corner_idx(cx + 1, cy, cz)] < 0.0) != d0 {
                    let q = [
                        vert(cx, cy - 1, cz - 1, 3),
                        vert(cx, cy, cz - 1, 2),
                        vert(cx, cy, cz, 0),
                        vert(cx, cy - 1, cz, 1),
                    ];
                    quad(q, d0, m);
                }
                // y edge -> quad in the x/z plane
                if (cv[corner_idx(cx, cy + 1, cz)] < 0.0) != d0 {
                    let q = [
                        vert(cx - 1, cy, cz - 1, 7),
                        vert(cx, cy, cz - 1, 6),
                        vert(cx, cy, cz, 4),
                        vert(cx - 1, cy, cz, 5),
                    ];
                    quad(q, !d0, m);
                }
                // z edge -> quad in the x/y plane
                if (cv[corner_idx(cx, cy, cz + 1)] < 0.0) != d0 {
                    let q = [
                        vert(cx - 1, cy - 1, cz, 11),
                        vert(cx, cy - 1, cz, 10),
                        vert(cx, cy, cz, 8),
                        vert(cx - 1, cy, cz, 9),
                    ];
                    quad(q, d0, m);
                }
            }
        }
    }

    // Close each cut sheet's slit: at each cut, a triangle from its two halves to the
    // vertex across the face it was cut at — or a quad, when the cell across was cut at
    // the same place. Each is emitted by the chunk owning the higher of the two cells
    // across that face, so it exists exactly once.
    let owned = |c: [i32; 3]| c.iter().all(|v| (0..hi).contains(v));
    for &(cell, ref cut) in &cuts {
        for (ex, ey) in cut.at {
            let Some(f) = FACES.iter().position(|(_, es)| es.contains(&ex) && es.contains(&ey)) else {
                continue;
            };
            let (axis, up) = (f / 2, f % 2 == 1);
            let mut across = cell;
            across[axis] += if up { 1 } else { -1 };
            if !owned(if up { across } else { cell }) {
                continue;
            }
            let (hx, hy) = (vert(cell[0], cell[1], cell[2], ex as u32), vert(cell[0], cell[1], cell[2], ey as u32));
            let ox = vert(across[0], across[1], across[2], mirror_edge(ex, axis) as u32);
            let oy = vert(across[0], across[1], across[2], mirror_edge(ey, axis) as u32);
            if [hx, hy, ox, oy].contains(&u32::MAX) {
                continue;
            }
            let tris: &[[u32; 3]] = if ox == oy {
                &[[hx, hy, ox]]
            } else if !up {
                // Both cells cut here; the higher one closes the four-sided slit.
                &[[hx, hy, oy], [hx, oy, ox]]
            } else {
                &[]
            };
            let flip = tris.first().is_some_and(|t| facing(m, t[0], t[1], t[2]) < 0.0);
            for t in tris {
                m.indices.extend_from_slice(&if flip { [t[1], t[0], t[2]] } else { *t });
            }
        }
    }
    };
    build(&mut m);

    // A vertex that settled onto ground its neighbours did not settle onto folds the
    // triangles between them over, and a folded triangle faces against its normals:
    // a dark shard. Such a vertex moves halfway to the middle of the vertices it is
    // joined to, a few times over, and the triangles are built again each time; one
    // still folded after that goes back to where the solid-keeping filter put it, a
    // little proud of the ground. Straight back the first time, a fold stood the
    // vertex up to most of a cell over the ground, and its neighbours folded and went
    // back after it: a stride-8 plateau 11 m over a shallow basin, where the fold was a
    // sliver. Only a vertex of this chunk's own cells: one in the outer layer is shared
    // with a neighbour, which has to place it the same way.
    const UNFOLD_ROUNDS: usize = 6;
    for round in 0..UNFOLD_ROUNDS {
        let mut back: Vec<usize> = Vec::new();
        for t in m.indices.as_chunks::<3>().0 {
            if facing(&m, t[0], t[1], t[2]) < 0.0 {
                back.extend(t.iter().map(|v| *v as usize).filter(|v| !rim[*v] && unsettle[*v].is_some()));
            }
        }
        if back.is_empty() {
            break;
        }
        back.sort_unstable();
        back.dedup();
        if round + 1 < UNFOLD_ROUNDS {
            let mut ring = vec![(Vec3::ZERO, 0.0f32); m.positions.len()];
            for t in m.indices.as_chunks::<3>().0 {
                for k in 0..3 {
                    let (a, b) = (t[k] as usize, t[(k + 1) % 3] as usize);
                    ring[a].0 += Vec3::from(m.positions[b]);
                    ring[a].1 += 1.0;
                    ring[b].0 += Vec3::from(m.positions[a]);
                    ring[b].1 += 1.0;
                }
            }
            for v in back {
                let Some((_, dir)) = unsettle[v] else { continue };
                if ring[v].1 == 0.0 {
                    continue;
                }
                let p = (Vec3::from(m.positions[v]) + ring[v].0 / ring[v].1) * 0.5 + origin;
                m.positions[v] = (p - origin).to_array();
                m.normals[v] = vertex_normal(s, p, dir);
            }
        } else {
            for v in back {
                if let Some((raw, dir)) = unsettle[v].take() {
                    m.positions[v] = (raw - origin).to_array();
                    m.normals[v] = vertex_normal(s, raw, dir);
                }
            }
        }
        m.indices.clear();
        build(&mut m);
    }

    // A skirted coarse chunk caps, on its own faces, a passage its minimum closed.
    // A passage narrower than a coarse cell that runs out of a finer neighbour is
    // shut by the minimum at the shared face; the neighbour sees it open at full
    // detail and its tunnel simply stops at the face, with nothing on this side to
    // meet it, so you looked down the passage into the rock. The cap is a square
    // a stride across, on the face, wherever a corner there reads solid to this
    // chunk but its square is open at full detail on both sides of the face, and
    // only where its
    // four neighbours on the face are rock at full detail, so it closes a passage
    // and shows only through the neighbour's opening. A neighbour at
    // this same stride closes the passage just the same, which leaves the cap
    // buried.
    if overlap {
        let solid = |c: [i32; 3]| cv[corner_idx(c[0], c[1], c[2])] < 0.0;
        let open = |c: [i32; 3]| {
            s.at_fast([base[0] + c[0] * stride, base[1] + c[1] * stride, base[2] + c[2] * stride]) >= 0.0
        };
        // Open at full detail on both sides of the face somewhere in the square
        // the cap would cover, half a stride either way of the corner: a passage
        // anywhere across that square, not only one a corner happens to land in.
        let open_through = |c: [i32; 3], axis: usize, e1: usize, e2: usize, out: i32| {
            let half = stride / 2;
            let corner = [base[0] + c[0] * stride, base[1] + c[1] * stride, base[2] + c[2] * stride];
            (-half..=half).any(|a| {
                (-half..=half).any(|b| {
                    let mut v = corner;
                    v[e1] += a;
                    v[e2] += b;
                    let mut beyond = v;
                    beyond[axis] += out * stride;
                    s.at_fast(v) >= 0.0 && s.at_fast(beyond) >= 0.0
                })
            })
        };
        let h = 0.5 * stride as f32 * voxel;
        for axis in 0..3 {
            let (e1, e2) = ((axis + 1) % 3, (axis + 2) % 3);
            for (layer, out, toward) in [(0, -1, -1.0f32), (n, n + 1, 1.0)] {
                for u in 0..=n {
                    for w in 0..=n {
                        let at = |l: i32, u: i32, w: i32| {
                            let mut c = [0; 3];
                            c[axis] = l;
                            c[e1] = u;
                            c[e2] = w;
                            c
                        };
                        let c = at(layer, u, w);
                        if !solid(c) || !open_through(c, axis, e1, e2, out - layer) {
                            continue;
                        }
                        // Rock at full detail all round it on the face: a passage
                        // narrower than a cell, not the open air over a slope,
                        // where a cap would stand up out of the ground.
                        if ![(u - 1, w), (u + 1, w), (u, w - 1), (u, w + 1)].iter().all(|&(a, b)| !open(at(layer, a, b))) {
                            continue;
                        }
                        let unit = |k: usize| {
                            let mut v = Vec3::ZERO;
                            v[k] = 1.0;
                            v
                        };
                        let (x1, x2, nrm) = (unit(e1) * h, unit(e2) * h, unit(axis) * toward);
                        let centre = Vec3::new(c[0] as f32, c[1] as f32, c[2] as f32) * (stride as f32 * voxel);
                        let col = s.color_at([base[0] + c[0] * stride, base[1] + c[1] * stride, base[2] + c[2] * stride]);
                        let first = m.positions.len() as u32;
                        for p in [centre - x1 - x2, centre + x1 - x2, centre + x1 + x2, centre - x1 + x2] {
                            m.positions.push(p.to_array());
                            m.normals.push(nrm.to_array());
                            m.colors.push(col);
                            if s.edge_reach > 0 {
                                m.edges.push(s.slot_edge(p + origin, col[3]));
                            }
                            // Its edges end the mesh like a seam's: hung with a
                            // skirt back into the rock behind.
                            rim.push(true);
                        }
                        // x1 × x2 runs along +axis: wind that way to face +axis.
                        let [a, b, c, d] = [first, first + 1, first + 2, first + 3];
                        if toward > 0.0 {
                            m.indices.extend_from_slice(&[a, b, c, a, c, d]);
                        } else {
                            m.indices.extend_from_slice(&[a, c, b, a, d, c]);
                        }
                    }
                }
            }
        }
    }

    if skirt {
        add_skirt(&mut m, s, &rim);
    }
    m
}

/// A vertex's normal: the field's smooth gradient, one voxel either side, unless that
/// disagrees with the cell's own facing `cell_dir`. Across rock about a voxel thick
/// the two samples land in the air on both sides and the smooth gradient points along
/// the rock rather than out of it, and a triangle lit from behind shows as a dark shard.
fn vertex_normal(s: &MeshScratch, p: Vec3, cell_dir: Vec3) -> [f32; 3] {
    let mut nrm = s.grad(p);
    if cell_dir != Vec3::ZERO && nrm.dot(cell_dir) < 0.9 {
        nrm = cell_dir;
    }
    if nrm == Vec3::ZERO { [0.0, 1.0, 0.0] } else { nrm.to_array() }
}

/// How well triangle `a b c` faces the way the field does at its corners: the cosine
/// between its face normal and the sum of its vertex normals.
fn facing(m: &ChunkMesh, a: u32, b: u32, c: u32) -> f32 {
    let p = |i: u32| Vec3::from(m.positions[i as usize]);
    let n = |i: u32| Vec3::from(m.normals[i as usize]);
    let face = (p(b) - p(a)).cross(p(c) - p(a)).normalize_or_zero();
    face.dot((n(a) + n(b) + n(c)).normalize_or_zero())
}

/// Hang a curtain from every edge where the chunk's mesh ends, dropped along each
/// vertex's −normal into the solid, so the seam against a neighbour at another stride
/// can't show what is behind the ground.
///
/// `rim[v]` says whether vertex `v` belongs to a cell in the outermost layer on some
/// axis. The mesh ends only between two such vertices, so only their edges are counted.
fn add_skirt(m: &mut ChunkMesh, s: &MeshScratch, rim: &[bool]) {
    let surface_tris = m.indices.len() / 3;
    // An edge used by one triangle is where the mesh ends. Sorting packed keys finds them
    // without a hash map in the meshing hot path.
    let mut edges: Vec<(u64, u32, u32)> = Vec::new();
    for t in m.indices.as_chunks::<3>().0 {
        for k in 0..3 {
            let (a, b) = (t[k], t[(k + 1) % 3]);
            if rim[a as usize] && rim[b as usize] {
                edges.push(((a.min(b) as u64) << 32 | a.max(b) as u64, a, b));
            }
        }
    }
    edges.sort_unstable_by_key(|e| e.0);
    let origin = Vec3::from(m.origin);
    let reach = reach(s.stride) as f32 * s.voxel;
    let mut dropped = vec![u32::MAX; m.positions.len()];
    let mut below = |m: &mut ChunkMesh, v: u32| {
        if dropped[v as usize] != u32::MAX {
            return dropped[v as usize];
        }
        let p = Vec3::from(m.positions[v as usize]) + origin;
        let n = Vec3::from(m.normals[v as usize]);
        let q = p - n * s.depth_below(p, n, reach);
        let id = m.positions.len() as u32;
        m.positions.push((q - origin).to_array());
        m.normals.push(m.normals[v as usize]);
        m.colors.push(m.colors[v as usize]);
        if let Some(&e) = m.edges.get(v as usize) {
            m.edges.push(e);
        }
        dropped[v as usize] = id;
        id
    };
    let mut i = 0;
    while i < edges.len() {
        let mut j = i + 1;
        while j < edges.len() && edges[j].0 == edges[i].0 {
            j += 1;
        }
        if j - i == 1 {
            let (_, a, b) = edges[i];
            let (a2, b2) = (below(m, a), below(m, b));
            // The triangle runs a→b, so the surface beyond this edge would run b→a: the
            // curtain carries on with that winding, folded down into the solid.
            m.indices.extend_from_slice(&[b, a, a2, b, a2, b2]);
        }
        i = j;
    }
    m.skirt_tris = m.indices.len() / 3 - surface_tris;
}

/// Mesh every chunk holding data. Convenience for tests/tools; the editor drives
/// per-chunk remeshes from a dirty set instead.
pub fn mesh_field(field: &ChunkField, stride: i32) -> Vec<([i32; 3], ChunkMesh)> {
    let mut out = Vec::new();
    for c in field.chunk_coords() {
        let m = mesh_chunk(field, c, stride, false);
        if !m.is_empty() {
            out.push((c, m));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunks::ChunkField;
    use crate::{Brush, BrushProfile};
    use std::collections::HashMap;

    /// An analytic sphere written into the field — the reference shape for accuracy,
    /// because we know its true surface and true normals exactly.
    fn sphere_field(voxel: f32, radius: f32) -> ChunkField {
        let mut f = ChunkField::new(voxel);
        let r = (radius / voxel).ceil() as i32 + 6;
        for z in -r..=r {
            for y in -r..=r {
                for x in -r..=r {
                    let p = Vec3::new(x as f32, y as f32, z as f32) * voxel;
                    f.write_voxel([x, y, z], p.length() - radius);
                }
            }
        }
        f.compact_all();
        f
    }

    /// Triangles must wind counter-clockwise seen from outside the solid — i.e. each
    /// face's geometric normal (the cross product, which is what the rasterizer's
    /// `front_facing` is computed from) must agree with the outward field gradient.
    ///
    /// Nothing consumed winding until the P2 render swap, and it was globally inverted:
    /// every terrain triangle reported `front_facing == false`, so the shader flipped
    /// every shading normal and the whole terrain rendered ambient-black. The old
    /// `facing_normal`, which took its cue from the interpolated normal instead of the
    /// winding, had been quietly papering over it. Vertex normals being right (they are,
    /// to <5°) says nothing about winding — they are independent, so this is its own gate.
    /// **Every corner of undug ground carries the ground's own slot, with the
    /// subsoil a voxel under it.** The terrain shader draws a triangle whose
    /// corners agree from that slot alone; only a triangle whose corners
    /// differ is blended from the shadow atlas, whose cells are two or three
    /// voxels across and reached the subsoil through undug ground. So the
    /// corners must not reach it either.
    #[test]
    fn the_corners_of_undug_ground_carry_its_own_slot() {
        let f = crate::procgen::generate_planet(&crate::procgen::PlanetFill {
            seed: 5,
            radius: 100.0,
            voxel: 1.5,
            relief: 8.0,
            cave_depth: 0.0,
            subsoil_depth: 1.5,
            subsoil: crate::procgen::LayerPaint { slot: 9, color: [0.5, 0.5, 0.5] },
            ..Default::default()
        });
        let (mut corners, mut deeper) = (0usize, 0usize);
        for c in f.chunk_coords() {
            let m = mesh_chunk(&f, c, 1, false);
            for col in &m.colors {
                corners += 1;
                deeper += usize::from(!matches!(col[3], 1 | 2));
            }
        }
        assert!(corners > 10_000, "{corners} corners");
        // A handful on the steepest slopes, where the subsoil a voxel down by
        // the radius is less than a voxel down by the slope: real exposure.
        assert!(deeper * 1000 <= corners, "{deeper} of {corners} corners of undug ground carry a deeper layer's slot");
    }

    fn subsoil_planet() -> ChunkField {
        crate::procgen::generate_planet(&crate::procgen::PlanetFill {
            seed: 5,
            radius: 100.0,
            voxel: 1.5,
            relief: 8.0,
            cave_depth: 0.0,
            subsoil_depth: 1.5,
            subsoil: crate::procgen::LayerPaint { slot: 9, color: [0.5, 0.5, 0.5] },
            ..Default::default()
        })
    }

    /// **Undug ground finds no subsoil to blend toward.** With the edges baked,
    /// almost no vertex of undug ground pairs with the layer a voxel under it:
    /// only ground counts, and the subsoil is a voxel deep. (A handful on the
    /// steepest slopes, as for the corners above.) Every vertex has its edge.
    #[test]
    fn undug_ground_bakes_no_edge_toward_the_subsoil() {
        let mut f = subsoil_planet();
        f.set_slot_edge_reach(2);
        let (mut verts, mut toward_subsoil, mut exposed) = (0usize, 0usize, 0usize);
        for c in f.chunk_coords() {
            let m = mesh_chunk(&f, c, 1, false);
            assert_eq!(m.edges.len(), m.positions.len(), "an edge per vertex");
            for (e, col) in m.edges.iter().zip(&m.colors) {
                verts += 1;
                exposed += usize::from(col[3] == 9);
                let pair = e[1] as u32;
                toward_subsoil += usize::from(pair != 0 && col[3] != 9 && (pair % 256 == 9 || pair / 256 == 9));
            }
        }
        assert!(verts > 10_000, "{verts} vertices");
        assert!(toward_subsoil * 1000 <= verts, "{toward_subsoil} of {verts} vertices of undug ground blend toward the subsoil");
        // …and those are next to where the subsoil really is at the surface.
        assert!(toward_subsoil <= 4 * exposed + 10, "{toward_subsoil} vertices blend toward the subsoil, beside only {exposed} that show it");
    }

    /// **A dug pit's rim knows where the ground changes, between the vertices.**
    /// Round a pit dug through to the subsoil, the vertices on the subsoil side
    /// and on the surface side of the rim carry the same pair, with opposite
    /// signs: the boundary is where the distance crosses zero, which falls
    /// inside triangles, not along their edges. Off by default: no reach, no
    /// edges, and the mesh is what it always was.
    #[test]
    fn a_dug_pit_bakes_a_signed_edge_across_its_rim() {
        let mut f = subsoil_planet();
        let top = f.raycast(Vec3::new(0.0, 160.0, 0.0), Vec3::NEG_Y, 160.0).expect("the ground");
        for k in 0..4 {
            f.sculpt(Brush::Lower, top - Vec3::Y * (k as f32 * 0.8), 4.5, 1.0, BrushProfile::default());
        }
        let chunk = (top / (f.voxel() * CHUNK as f32)).floor();
        let c = [chunk.x as i32, chunk.y as i32, chunk.z as i32];
        assert!(mesh_chunk(&f, c, 1, false).edges.is_empty(), "no reach bakes no edges");
        f.set_slot_edge_reach(2);
        let (mut neg, mut pos) = (0usize, 0usize);
        for dz in -1..=1 {
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let m = mesh_chunk(&f, [c[0] + dx, c[1] + dy, c[2] + dz], 1, false);
                    for (e, col) in m.edges.iter().zip(&m.colors) {
                        let pair = e[1] as u32;
                        if pair == 0 || pair / 256 != 9 {
                            continue;
                        }
                        // The pair's hi is the subsoil: its side is positive.
                        if col[3] == 9 {
                            assert!(e[0] >= 0.0, "a subsoil vertex on the negative side: {e:?}");
                            pos += 1;
                        } else {
                            assert!(e[0] <= 0.0, "a surface vertex on the subsoil's side: {e:?}");
                            neg += 1;
                        }
                    }
                }
            }
        }
        assert!(neg > 10 && pos > 10, "the rim has {neg} surface and {pos} subsoil vertices that know the edge");
    }

    /// **A box of the surface is the chunk's surface there.** A ray asking
    /// where the drawn ground is needs a few cells of it, not a chunk: the
    /// box's triangles are the chunk mesh's, for every edge whose low corner is
    /// in the box (to a thousandth of a unit: each mesh places its vertices
    /// from its own origin).
    #[test]
    fn a_box_of_the_surface_is_the_chunks_surface_there() {
        let f = solar_planet();
        let key = |p: Vec3| p.to_array().map(|v| (v * 1000.0).round() as i32 as u32);
        let tris = |m: &ChunkMesh| -> std::collections::HashSet<[[u32; 3]; 3]> {
            let o = Vec3::from(m.origin);
            m.indices[..(m.tri_count() - m.skirt_tris) * 3]
                .chunks(3)
                .map(|t| {
                    let mut v = [0usize, 1, 2].map(|k| key(Vec3::from(m.positions[t[k] as usize]) + o));
                    v.sort();
                    v
                })
                .collect()
        };
        let mut boxes = 0;
        for d in directions(40) {
            let Some(hit) = f.raycast(d * 170.0, -d, 170.0) else { continue };
            let v = (hit / f.voxel()).floor();
            let base = [v.x as i32 - 3, v.y as i32 - 3, v.z as i32 - 3];
            let c = base.map(|b| b.div_euclid(CHUNK));
            // Inside one chunk, so the chunk's mesh holds every edge the box does.
            if base.iter().zip(c).any(|(b, c)| b + 6 > (c + 1) * CHUNK) {
                continue;
            }
            let small = tris(&mesh_box(f, base, 6));
            let whole = tris(&mesh_chunk(f, c, 1, false));
            assert!(!small.is_empty(), "no surface in the box at {hit:?}");
            for t in &small {
                assert!(whole.contains(t), "a box triangle at {hit:?} is not in its chunk's mesh");
            }
            boxes += 1;
        }
        assert!(boxes > 10, "only {boxes} boxes inside one chunk");
    }

    #[test]
    fn triangles_wind_outward() {
        let (voxel, radius) = (1.0f32, 8.0f32);
        let f = sphere_field(voxel, radius);
        let meshes = mesh_field(&f, 1);
        assert!(!meshes.is_empty(), "a sphere must produce geometry");
        let (mut ok, mut total) = (0u32, 0u32);
        for (_, m) in &meshes {
            let o = Vec3::from(m.origin);
            for t in m.indices.as_chunks::<3>().0 {
                let p: Vec<Vec3> = t.iter().map(|&i| Vec3::from(m.positions[i as usize]) + o).collect();
                let face = (p[1] - p[0]).cross(p[2] - p[0]);
                if face.length_squared() < 1e-12 {
                    continue; // degenerate sliver — carries no winding
                }
                // The sphere is centred at the origin, so "outward" is just the position.
                let outward = (p[0] + p[1] + p[2]) / 3.0;
                total += 1;
                if face.normalize().dot(outward.normalize()) > 0.0 {
                    ok += 1;
                }
            }
        }
        let pct = ok as f32 / total as f32 * 100.0;
        assert!(pct > 99.0, "only {pct:.1}% of {total} triangles wind outward");
    }

    /// The whole field's mesh — every chunk welded together by world position — must have
    /// no boundary edges at all. A closed sphere is a closed surface.
    ///
    /// `sphere_mesh_is_watertight` cannot see this: it inspects one chunk, where the cut
    /// against neighbours legitimately leaves boundary edges, so it has to tolerate them
    /// (<30%). Holes therefore hid in plain sight. They matter now that terrain is
    /// rasterized: a hole in the near surface exposes the solid's inside face, which the
    /// raymarch never showed because it hit the field itself.
    #[test]
    fn the_assembled_field_mesh_has_no_holes() {
        let f = sphere_field(1.0, 8.0);
        let meshes = mesh_field(&f, 1);
        // Weld by quantized world position: chunks meet exactly (T3), so shared-boundary
        // vertices land on identical coordinates and collapse to one id.
        let key = |p: Vec3| {
            [
                (p.x * 1024.0).round() as i64,
                (p.y * 1024.0).round() as i64,
                (p.z * 1024.0).round() as i64,
            ]
        };
        let mut ids: HashMap<[i64; 3], u32> = HashMap::new();
        let mut edges: HashMap<(u32, u32), i32> = HashMap::new();
        for (_, m) in &meshes {
            let o = Vec3::from(m.origin);
            let mut local = Vec::with_capacity(m.positions.len());
            for p in &m.positions {
                let k = key(Vec3::from(*p) + o);
                let n = ids.len() as u32;
                local.push(*ids.entry(k).or_insert(n));
            }
            for t in m.indices.as_chunks::<3>().0 {
                for k in 0..3 {
                    let (a, b) = (local[t[k] as usize], local[t[(k + 1) % 3] as usize]);
                    if a == b {
                        continue; // degenerate sliver edge
                    }
                    *edges.entry((a.min(b), a.max(b))).or_insert(0) += 1;
                }
            }
        }
        let boundary = edges.values().filter(|c| **c == 1).count();
        let over = edges.values().filter(|c| **c > 2).count();
        assert_eq!(over, 0, "{over} edges shared by >2 triangles — non-manifold");
        assert_eq!(boundary, 0, "{boundary}/{} edges are HOLES in a closed sphere", edges.len());
    }

    /// Every vertex must sit on the surface it claims to represent.
    #[test]
    fn sphere_vertices_land_on_the_true_surface() {
        let (voxel, radius) = (1.0f32, 8.0f32);
        let f = sphere_field(voxel, radius);
        let meshes = mesh_field(&f, 1);
        assert!(!meshes.is_empty(), "a sphere must produce geometry");
        let (mut worst, mut n) = (0.0f32, 0u32);
        for (_, m) in &meshes {
            for p in &m.positions {
                let w = Vec3::from(*p) + Vec3::from(m.origin);
                worst = worst.max((w.length() - radius).abs());
                n += 1;
            }
        }
        assert!(n > 200, "expected a real vertex population, got {n}");
        assert!(worst < 0.3 * voxel, "worst vertex off the sphere by {worst:.3} (> 0.3 voxel)");
    }

    /// Normals must come from the field, not the triangles — this is the property that
    /// retires the up-close faceting the whole redesign exists to kill.
    #[test]
    fn sphere_normals_match_the_analytic_normal() {
        let (voxel, radius) = (1.0f32, 8.0f32);
        let f = sphere_field(voxel, radius);
        let mut worst_deg = 0.0f32;
        for (_, m) in mesh_field(&f, 1) {
            for (p, nr) in m.positions.iter().zip(&m.normals) {
                let w = Vec3::from(*p) + Vec3::from(m.origin);
                let truth = w.normalize_or_zero();
                let got = Vec3::from(*nr).normalize_or_zero();
                let deg = truth.dot(got).clamp(-1.0, 1.0).acos().to_degrees();
                worst_deg = worst_deg.max(deg);
            }
        }
        assert!(worst_deg < 5.0, "worst vertex normal off by {worst_deg:.1}° (> 5°)");
    }

    /// Watertight: every interior edge is shared by exactly two triangles. A hole here
    /// means background pixels through the terrain.
    #[test]
    fn sphere_mesh_is_watertight() {
        let f = sphere_field(1.0, 8.0);
        let mut edges: HashMap<(u32, u32), i32> = HashMap::new();
        // One chunk at a time: cross-chunk welding is the seam test's job, not this one.
        let meshes = mesh_field(&f, 1);
        let (_, m) = meshes
            .iter()
            .max_by_key(|(_, m)| m.tri_count())
            .expect("some chunk has triangles");
        for t in m.indices.chunks(3) {
            for k in 0..3 {
                let (a, b) = (t[k], t[(k + 1) % 3]);
                *edges.entry((a.min(b), a.max(b))).or_insert(0) += 1;
            }
        }
        // Interior edges have 2 uses; the chunk's cut boundary leaves some with 1.
        let interior_bad = edges.values().filter(|c| **c > 2).count();
        assert_eq!(interior_bad, 0, "{interior_bad} edges shared by >2 triangles — non-manifold");
        let boundary = edges.values().filter(|c| **c == 1).count();
        let total = edges.len();
        assert!(
            (boundary as f32) < 0.30 * total as f32,
            "{boundary}/{total} edges are boundary — the chunk interior isn't closed"
        );
    }

    /// Two adjacent chunks must agree exactly on their shared boundary, or seams shade
    /// visibly (trap T3). This is the payoff of addressing the field by global voxel
    /// index rather than chunk-local arrays.
    #[test]
    fn adjacent_chunks_agree_on_the_seam() {
        // A sphere big enough to straddle several chunks.
        let f = sphere_field(1.0, 40.0);
        let meshes: HashMap<[i32; 3], ChunkMesh> = mesh_field(&f, 1).into_iter().collect();
        assert!(meshes.len() > 1, "test needs a multi-chunk surface, got {}", meshes.len());

        // Collect world-space vertices per chunk, then check that vertices near a shared
        // face have a partner in the neighbour at the same place with the same normal.
        let world: HashMap<[i32; 3], Vec<(Vec3, Vec3)>> = meshes
            .iter()
            .map(|(c, m)| {
                let o = Vec3::from(m.origin);
                (
                    *c,
                    m.positions
                        .iter()
                        .zip(&m.normals)
                        .map(|(p, n)| (Vec3::from(*p) + o, Vec3::from(*n)))
                        .collect(),
                )
            })
            .collect();

        let mut checked = 0;
        for (c, verts) in &world {
            let nb = [c[0] + 1, c[1], c[2]];
            let Some(other) = world.get(&nb) else { continue };
            // Chunk c's +x face plane, in world units.
            let face_x = (c[0] + 1) as f32 * CHUNK as f32 * f.voxel();
            for (p, n) in verts {
                if (p.x - face_x).abs() > f.voxel() * 0.75 {
                    continue;
                }
                // The neighbour must have a vertex within a voxel with a matching normal:
                // both chunks sampled the same field voxels to build it.
                let best = other
                    .iter()
                    .filter(|(q, _)| (*q - *p).length() < f.voxel() * 1.5)
                    .map(|(_, m)| m.dot(*n))
                    .fold(f32::NEG_INFINITY, f32::max);
                if best > f32::NEG_INFINITY {
                    let deg = best.clamp(-1.0, 1.0).acos().to_degrees();
                    assert!(deg < 8.0, "seam normals disagree by {deg:.1}° at {p:?}");
                    checked += 1;
                }
            }
        }
        assert!(checked > 20, "seam test didn't examine enough shared vertices ({checked})");
    }

    /// Realistic sculpted terrain, and the guard is a ratio rather than a duration.
    ///
    /// The paint-brush freeze taught us that perf tests on synthetic shapes pass while
    /// real content hangs (T7), so the field is a slab with 24 real brush strokes on it.
    ///
    /// ## Why this is not an absolute millisecond bound any more
    ///
    /// It was, and it flaked: 7.23 ms against a 6 ms bound on a CI runner, while the
    /// same commit passed in a run that was not sharing the machine. The old comment
    /// said "a timing assert that flips with the scheduler is worse than no assert" —
    /// which was right, and the bound was simply not high enough to make it true. Any
    /// bound high enough to survive a loaded shared runner is also high enough to let a
    /// 3× regression through.
    ///
    /// So the measurement is a ratio of two timings taken in the same run, which makes
    /// runner speed cancel exactly.
    ///
    /// ## What the two timings are
    ///
    /// Meshing a chunk is a fixed voxel scan (visit every voxel to find the surface)
    /// plus per-vertex gradient work. Measuring both separates them:
    ///
    /// * an **empty** chunk — full voxel count, zero vertices — is the scan alone;
    /// * the **busiest** chunk is the scan plus every vertex.
    ///
    /// So `(busiest − empty) / empty` is the gradient's cost expressed in units of the
    /// scan's, and it is dimensionless. Measured ~0.28 here (0.47 ms of gradient over a
    /// 1.69 ms scan, 3,178 triangles ⇒ ~0.15 µs/tri).
    ///
    /// The regression this exists to catch is per-voxel HashMap lookups in the gradient,
    /// which measured 11 ms for a chunk — the same ratio would be ~5.5, twenty times the
    /// healthy figure. A bound of 2.0 sits with 7× headroom above healthy and still trips
    /// that by 2.7×, and no amount of scheduler noise moves it, because noise scales both
    /// timings together.
    #[test]
    fn the_gradient_costs_a_fraction_of_the_voxel_scan_it_rides_on() {
        let mut f = ChunkField::new(1.5);
        f.fill_slab(Vec3::new(-60.0, -20.0, -60.0), Vec3::new(60.0, 20.0, 60.0), 0.0, [0.4, 0.6, 0.3]);
        for i in 0..24 {
            let a = i as f32 * 2.399;
            f.sculpt(
                Brush::Raise,
                Vec3::new(a.cos() * 30.0, 2.0, a.sin() * 30.0),
                9.0,
                0.9,
                BrushProfile::default(),
            );
        }
        let coords = f.chunk_coords();
        assert!(!coords.is_empty());

        let tris_of = |c: [i32; 3]| mesh_chunk(&f, c, 1, false).tri_count();
        // The busiest chunk, not the average — the average would flatter us.
        let busiest = coords.iter().copied().max_by_key(|c| tris_of(*c)).unwrap();
        // …and an empty one: same voxel count, no vertices. A sculpted slab always has
        // chunks entirely above or below the surface; if it somehow did not, there is
        // nothing to subtract and the ratio would be meaningless, so say so.
        let empty = coords.iter().copied().find(|c| tris_of(*c) == 0);
        let Some(empty) = empty else {
            panic!("no empty chunk to measure the bare voxel scan against — the field \
                    changed shape and this test needs a new baseline");
        };
        let busy_tris = tris_of(busiest);
        assert!(busy_tris > 500, "the busiest chunk has only {busy_tris} triangles to time");

        // Interleave the two timings so a runner that slows down partway through
        // affects both, rather than whichever happened to run during the slow patch.
        let reps = 20;
        let mut t_empty = 0.0f64;
        let mut t_busy = 0.0f64;
        for _ in 0..reps {
            let a = std::time::Instant::now();
            std::hint::black_box(mesh_chunk(&f, empty, 1, false));
            t_empty += a.elapsed().as_secs_f64();
            let b = std::time::Instant::now();
            std::hint::black_box(mesh_chunk(&f, busiest, 1, false));
            t_busy += b.elapsed().as_secs_f64();
        }
        let (scan_ms, busy_ms) = (t_empty * 1000.0 / reps as f64, t_busy * 1000.0 / reps as f64);
        let gradient_ms = (busy_ms - scan_ms).max(0.0);
        let ratio = gradient_ms / scan_ms.max(1e-9);
        println!(
            "voxel scan {scan_ms:.3} ms · busiest chunk {busy_ms:.3} ms ({busy_tris} tris) \
             ⇒ gradient {gradient_ms:.3} ms = {ratio:.2}× the scan ({:.3} µs/tri)",
            gradient_ms * 1000.0 / busy_tris as f64
        );
        assert!(
            ratio < 2.0,
            "the gradient now costs {ratio:.2}× the voxel scan it rides on (healthy is \
             ~0.3×) — that is the shape of a per-VOXEL lookup where a per-vertex one \
             belongs, which measured 11 ms/chunk the last time it happened"
        );
        // A second, enormous absolute bound. The ratio cannot catch a
        // regression that slows the scan and the gradient equally; nothing in this
        // subsystem should ever take a tenth of a second for one chunk on any machine.
        assert!(
            busy_ms < 100.0,
            "one chunk took {busy_ms:.1} ms — something has gone wrong at a scale the \
             ratio cannot see"
        );
    }

    /// LOD strides must produce progressively cheaper meshes of the same surface.
    #[test]
    fn lod_strides_shed_triangles_and_still_hold_the_surface() {
        let f = sphere_field(1.0, 24.0);
        let mut prev = usize::MAX;
        for stride in [1, 2, 4] {
            let tris: usize = mesh_field(&f, stride).iter().map(|(_, m)| m.tri_count()).sum();
            assert!(tris > 0, "stride {stride} produced no geometry");
            assert!(tris < prev, "stride {stride}: {tris} tris did not shrink from {prev}");
            prev = tris;
            // …and the coarse mesh must still describe the same sphere.
            let mut worst = 0.0f32;
            for (_, m) in mesh_field(&f, stride) {
                for p in &m.positions {
                    let w = Vec3::from(*p) + Vec3::from(m.origin);
                    worst = worst.max((w.length() - 24.0).abs());
                }
            }
            assert!(worst < 1.2 * stride as f32, "stride {stride}: vertex off surface by {worst:.2}");
        }
    }

    /// The planet the see-through and folded-triangle reports were measured on: a game's
    /// walkable-planet settings on a 150-unit body, caves and all. Generated once for
    /// every test that wants it — it takes about ten seconds.
    fn solar_planet() -> &'static ChunkField {
        static PLANET: std::sync::OnceLock<ChunkField> = std::sync::OnceLock::new();
        PLANET.get_or_init(|| {
            let radius = 150.0;
            crate::procgen::generate_planet(&crate::procgen::PlanetFill {
                seed: 11,
                radius,
                voxel: 1.5,
                relief: radius * 0.065,
                bump_freq: 4.5,
                cave_depth: radius * 0.35,
                core_r: 10.5,
                ..Default::default()
            })
        })
    }

    /// A chunk's triangles in field space, with a box around them for a quick reject.
    struct Hull {
        lo: Vec3,
        hi: Vec3,
        tris: Vec<[Vec3; 3]>,
    }

    fn hull(m: &ChunkMesh) -> Hull {
        let o = Vec3::from(m.origin);
        let (mut lo, mut hi) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        let tris = m
            .indices
            .as_chunks::<3>()
            .0
            .iter()
            .map(|t| {
                let p = t.map(|i| Vec3::from(m.positions[i as usize]) + o);
                for v in p {
                    lo = lo.min(v);
                    hi = hi.max(v);
                }
                p
            })
            .collect();
        Hull { lo, hi, tris }
    }

    /// Distance along the ray to the first triangle it meets — either face; terrain is
    /// drawn without culling.
    fn first_hit(hulls: &[Hull], ro: Vec3, rd: Vec3, tmax: f32) -> Option<f32> {
        let mut best: Option<f32> = None;
        for h in hulls {
            let inv = Vec3::ONE / rd;
            let t0 = (h.lo - Vec3::splat(0.01) - ro) * inv;
            let t1 = (h.hi + Vec3::splat(0.01) - ro) * inv;
            let (tn, tf) = (t0.min(t1).max_element(), t0.max(t1).min_element());
            if tf < tn.max(0.0) || tn > best.unwrap_or(tmax) {
                continue;
            }
            for [a, b, c] in &h.tris {
                let (e1, e2) = (*b - *a, *c - *a);
                let p = rd.cross(e2);
                let det = e1.dot(p);
                if det.abs() < 1e-12 {
                    continue;
                }
                let s = ro - *a;
                let u = s.dot(p) / det;
                let q = s.cross(e1);
                let v = rd.dot(q) / det;
                let t = e2.dot(q) / det;
                if u >= -1e-5 && v >= -1e-5 && u + v <= 1.0 + 1e-5 && t > 1e-4 && t < best.unwrap_or(tmax) {
                    best = Some(t);
                }
            }
        }
        best
    }

    /// Evenly spread, repeatable ray directions.
    fn directions(n: usize) -> Vec<Vec3> {
        let golden = std::f32::consts::PI * (3.0 - 5f32.sqrt());
        (0..n)
            .map(|i| {
                let y = 1.0 - 2.0 * (i as f32 + 0.5) / n as f32;
                let r = (1.0 - y * y).sqrt();
                let a = golden * i as f32;
                Vec3::new(r * a.cos(), y, r * a.sin())
            })
            .collect()
    }

    /// Welded edge use counts over a set of triangles, keyed by world position.
    fn weld_edges<'a>(tris: impl Iterator<Item = [Vec3; 3]> + 'a) -> HashMap<([i64; 3], [i64; 3]), u32> {
        let key = |p: Vec3| [(p.x * 1024.0).round() as i64, (p.y * 1024.0).round() as i64, (p.z * 1024.0).round() as i64];
        let mut edges = HashMap::new();
        for t in tris {
            for k in 0..3 {
                let (a, b) = (key(t[k]), key(t[(k + 1) % 3]));
                if a != b {
                    *edges.entry((a.min(b), a.max(b))).or_insert(0) += 1;
                }
            }
        }
        edges
    }

    /// A dug tunnel under a thin roof stays roofed at every stride, seen from straight
    /// above.
    ///
    /// Coarse chunks used to sample the field only at their corners, so rock thinner
    /// than a coarse cell fell between samples while the tunnel's floor was still meshed:
    /// from a distance the player looked straight down into the tunnel, and walking up
    /// to it swapped in a solid roof. Measured before the fix: every ray through a
    /// 1.5–2.25 m roof at stride 2, and through a 4.5 m roof at stride 4.
    #[test]
    fn thin_rock_over_a_tunnel_stays_solid_at_every_stride() {
        let mut f = ChunkField::new(1.5);
        f.fill_slab(Vec3::new(-20.0, -40.0, -10.0), Vec3::new(70.0, 0.0, 80.0), 0.0, [0.5, 0.5, 0.5]);
        // A player's dig: a ball of radius 2, swept along x, its roof `crust` below the top.
        let crusts = [1.5f32, 2.25, 3.0, 4.5, 6.0];
        let row_z = |i: usize| 5.0 + i as f32 * 14.0;
        let hard = BrushProfile { hardness: 1.0, ..Default::default() };
        for (i, crust) in crusts.iter().enumerate() {
            let mut x = 5.0;
            while x < 50.0 {
                f.sculpt(Brush::Lower, Vec3::new(x, -(crust + 2.0), row_z(i)), 2.0, 1.0, hard);
                x += 0.75;
            }
        }
        for stride in [1, 2, 4, 8] {
            let hulls: Vec<Hull> = f
                .chunk_coords()
                .into_iter()
                .map(|c| hull(&mesh_chunk(&f, c, stride, true)))
                .collect();
            for (i, crust) in crusts.iter().enumerate() {
                let (mut rays, mut through) = (0, 0);
                let mut x = 10.0;
                while x < 45.0 {
                    for dz in [-1.0f32, -0.5, 0.0, 0.5, 1.0] {
                        let ro = Vec3::new(x, 20.0, row_z(i) + dz);
                        rays += 1;
                        // The roof spans y = -crust..0; a first hit below it is the
                        // tunnel showing through.
                        match first_hit(&hulls, ro, Vec3::NEG_Y, 100.0) {
                            Some(t) if ro.y - t > -crust => {}
                            _ => through += 1,
                        }
                    }
                    x += 0.5;
                }
                assert_eq!(
                    through, 0,
                    "stride {stride}: {through} of {rays} rays see through a {crust} m roof"
                );
            }
        }
    }

    /// Seen from straight overhead, a coarse chunk's ground sits on the real ground, not
    /// behind it and not floating over it.
    ///
    /// Point-sampled, a stride-8 chunk lost ridges and roofs thinner than its 12-unit
    /// cells, so the ray went on to whatever was meshed behind them: 7% of radial rays
    /// on this planet landed more than 5 units behind the true surface. Keeping that
    /// rock pushes the coarse surface outward, by 8.7 units on average here, until each
    /// vertex settles back onto the ground.
    #[test]
    fn coarse_ground_lands_within_one_cell_of_the_real_ground() {
        let f = solar_planet();
        let stride = 8;
        let hulls: Vec<Hull> =
            f.chunk_coords().into_iter().map(|c| hull(&mesh_chunk(f, c, stride, false))).collect();
        let top = 170.0;
        let cell = stride as f32 * f.voxel();
        let (mut rays, mut behind, mut worst, mut off) = (0, 0, 0.0f32, 0.0f32);
        for d in directions(1500) {
            let ro = d * top;
            let Some(truth) = f.raycast(ro, -d, top) else { continue };
            let t_true = (truth - ro).length();
            rays += 1;
            let t_mesh = first_hit(&hulls, ro, -d, top * 2.0).unwrap_or(f32::MAX);
            worst = worst.max(t_mesh - t_true);
            off += (t_mesh - t_true).abs().min(cell * 4.0);
            if t_mesh - t_true > cell {
                behind += 1;
            }
        }
        let mean_off = off / rays.max(1) as f32;
        assert!(rays > 1000, "only {rays} rays found the planet");
        assert_eq!(
            behind, 0,
            "{behind} of {rays} radial rays land more than one {cell}-unit cell behind the \
             ground (worst {worst:.1})"
        );
        assert!(
            mean_off < 0.1 * cell,
            "the coarse ground sits {mean_off:.2} units off the real ground on average \
             (limit a tenth of a {cell}-unit cell)"
        );
    }

    /// Coarse ground over a shallow gallery sits on the ground, not floating over it.
    ///
    /// A coarse vertex settles back onto the real ground only where the ground faces
    /// the way the coarse surface does, and that facing was judged half a stride away:
    /// over a crust a few metres thick the samples reach the gallery under it, the
    /// settle was refused, and the vertex stayed where the solid-keeping filter had
    /// pushed it, up to 9 m up. Tilted ground, as on a planet, a 4.5 m gallery 3 m
    /// down, read straight down the ground's normal.
    #[test]
    fn coarse_ground_over_a_shallow_gallery_stays_on_the_ground() {
        let voxel = 1.5;
        let (depth, radius) = (3.0f32, 4.5f32);
        let mut failures = Vec::new();
        for case in 0..10 {
            // Ten placements against the coarse lattice, at three tilts.
            let tilt = [0.3f32, 0.45, 0.6][case % 3];
            let n = Vec3::new(tilt, 1.0, 0.15 + 0.05 * case as f32).normalize();
            let u = n.cross(Vec3::Z).normalize();
            let v = n.cross(u);
            let origin = Vec3::new(case as f32 * 3.7, case as f32 * 1.3, case as f32 * 2.9);
            let axis = origin - n * (depth + radius);
            let mut f = ChunkField::new(voxel);
            f.fill_with(
                origin - Vec3::splat(60.0),
                origin + Vec3::splat(60.0),
                |p| {
                    let ground = (p - origin).dot(n);
                    // An infinite gallery along `u` under the ground.
                    let q = p - axis;
                    let off = q - u * q.dot(u);
                    ground.max(-(off.length() - radius))
                },
                |_| [0.5, 0.5, 0.5],
            );
            for stride in [4, 8] {
                let hulls: Vec<Hull> =
                    f.chunk_coords().into_iter().map(|c| hull(&mesh_chunk(&f, c, stride, false))).collect();
                let (mut rays, mut off, mut worst) = (0, 0, 0.0f32);
                let mut s = -30.0;
                while s <= 30.0 {
                    let mut w = -7.0;
                    while w <= 7.0 {
                        let ro = origin + u * s + v * w + n * 20.0;
                        w += 0.5;
                        let Some(truth) = f.raycast(ro, -n, 40.0) else { continue };
                        let Some(t) = first_hit(&hulls, ro, -n, 40.0) else { continue };
                        let gap = (t - (truth - ro).length()).abs();
                        rays += 1;
                        worst = worst.max(gap);
                        if gap > voxel {
                            off += 1;
                        }
                    }
                    s += 0.75;
                }
                assert!(rays > 1000, "case {case} stride {stride}: only {rays} rays");
                if off > 0 {
                    failures.push(format!("case {case} stride {stride}: {off} of {rays} (worst {worst:.2} m)"));
                }
            }
        }
        assert!(failures.is_empty(), "rays meeting the coarse ground more than a voxel off the real ground: {failures:#?}");
    }

    /// The meshes the editor draws for a body seen from its surface — every chunk at
    /// the stride its ring gives it — leave no gap at a change of stride.
    ///
    /// Each chunk builds its −x/−y/−z border from its neighbour's cells at its own
    /// stride, so where a coarse chunk met a finer one on its + side the two meshes
    /// ended on different lines and left a slit open to the sky. The skirt meant to hide
    /// it repeated a vertex, so every skirt triangle had zero area, and only coarse
    /// chunks had one. Measured before the fix: 4,112 open edges in this set, against
    /// 16 for the same planet meshed at one stride.
    #[test]
    fn a_mixed_stride_planet_has_no_gap_at_a_change_of_stride() {
        let f = solar_planet();
        let chunk = CHUNK as f32 * f.voxel();
        // The rings the editor gives a 150-unit body with 48-unit chunks
        // (`rings_for_body` in the editor's terrain module).
        let rings = [1, 2, 4];
        let up = Vec3::new(0.3, 0.9, 0.1).normalize();
        let ground = f.raycast(up * 170.0, -up, 170.0).expect("the planet has a surface");
        let eye = ground + up * 1.8;
        let eye_chunk = [(eye.x / chunk).floor() as i32, (eye.y / chunk).floor() as i32, (eye.z / chunk).floor() as i32];
        let lod_of = |c: [i32; 3]| {
            let d = (0..3).map(|k| (c[k] - eye_chunk[k]).abs()).max().unwrap_or(0);
            rings.iter().position(|r| d <= *r).unwrap_or(3)
        };
        let coords = f.chunk_coords();
        let lods: HashMap<[i32; 3], usize> = coords.iter().map(|c| (*c, lod_of(*c))).collect();
        let meshes: Vec<ChunkMesh> =
            coords.iter().map(|c| mesh_chunk(f, *c, 1 << lods[c], true)).collect();
        assert!(lods.values().any(|l| *l == 0) && lods.values().any(|l| *l == 3), "the view needs every stride");

        // Skirts have area.
        let tri = |m: &ChunkMesh, t: &[u32; 3]| t.map(|i| Vec3::from(m.positions[i as usize]) + Vec3::from(m.origin));
        let mut skirts = 0;
        for m in &meshes {
            let first = m.tri_count() - m.skirt_tris;
            for t in &m.indices.as_chunks::<3>().0[first..] {
                let [a, b, c] = tri(m, t);
                skirts += 1;
                assert!((b - a).cross(c - a).length() > 1e-6, "a skirt triangle has no area");
            }
        }
        assert!(skirts > 1000, "only {skirts} skirt triangles");

        // Every edge where the welded ground ends is hung with a skirt.
        let surface = weld_edges(meshes.iter().flat_map(|m| {
            m.indices.as_chunks::<3>().0[..m.tri_count() - m.skirt_tris].iter().map(move |t| tri(m, t))
        }));
        let skirted = weld_edges(meshes.iter().flat_map(|m| {
            m.indices.as_chunks::<3>().0[m.tri_count() - m.skirt_tris..].iter().map(move |t| tri(m, t))
        }));
        let open: Vec<_> = surface.iter().filter(|(_, n)| **n == 1).map(|(e, _)| *e).collect();
        let bare = open.iter().filter(|e| !skirted.contains_key(*e)).count();
        assert_eq!(bare, 0, "{bare} of {} open edges have no skirt", open.len());

        // And looking straight down at a change of stride, the ground is there: within
        // one coarse cell of the real surface, not through a slit to what is behind it.
        let hulls: Vec<Hull> = meshes.iter().map(hull).collect();
        let (mut rays, mut through, mut worst) = (0, 0, 0.0f32);
        for d in directions(12000) {
            let ro = d * 170.0;
            let Some(truth) = f.raycast(ro, -d, 170.0) else { continue };
            let c = [(truth.x / chunk).floor() as i32, (truth.y / chunk).floor() as i32, (truth.z / chunk).floor() as i32];
            let Some(&here) = lods.get(&c) else { continue };
            // The coarsest stride across any face of this chunk the hit is near.
            let mut coarse = 0;
            for k in 0..3 {
                let along = truth[k] / chunk - c[k] as f32;
                for (side, near) in [(-1, along), (1, 1.0 - along)] {
                    let mut n = c;
                    n[k] += side;
                    if let Some(&there) = lods.get(&n)
                        && there != here
                        && near * CHUNK as f32 <= (1 << here.max(there)) as f32
                    {
                        coarse = coarse.max(here.max(there));
                    }
                }
            }
            if coarse == 0 {
                continue;
            }
            rays += 1;
            let t_true = (truth - ro).length();
            let t_mesh = first_hit(&hulls, ro, -d, 400.0).unwrap_or(f32::MAX);
            worst = worst.max(t_mesh - t_true);
            if t_mesh - t_true > (1 << coarse) as f32 * f.voxel() {
                through += 1;
            }
        }
        assert!(rays > 100, "only {rays} rays landed near a change of stride");
        assert_eq!(through, 0, "{through} of {rays} rays at a change of stride pass the ground (worst {worst:.1} units)");
    }

    /// A generated planet meshes into a manifold surface whose triangles face the way
    /// the field does, at full detail and at stride 4.
    ///
    /// One vertex per cell pinched every cell holding two sheets of surface — a cave
    /// roof and the ground above it, both faces of a thin ridge — into one point, and
    /// the quads around it folded through each other. A fixed quad diagonal folded more,
    /// and normals sampled a voxel either side read across thin rock. Measured before
    /// the fix: 2,470 non-manifold edges and 0.40% of triangles facing against their
    /// vertex normals at stride 1; 1,100 and 3.4% at stride 4.
    #[test]
    fn a_generated_planet_meshes_manifold_with_faces_that_follow_the_field() {
        let f = solar_planet();
        for stride in [1, 4] {
            let meshes: Vec<ChunkMesh> =
                f.chunk_coords().into_iter().map(|c| mesh_chunk(f, c, stride, false)).collect();
            let (mut tris, mut against) = (0usize, 0usize);
            for m in &meshes {
                for t in m.indices.as_chunks::<3>().0 {
                    let p = t.map(|i| Vec3::from(m.positions[i as usize]));
                    let n: Vec3 = t.iter().map(|i| Vec3::from(m.normals[*i as usize])).sum();
                    tris += 1;
                    if (p[1] - p[0]).cross(p[2] - p[0]).normalize_or_zero().dot(n.normalize_or_zero()) < 0.0 {
                        against += 1;
                    }
                }
            }
            let edges = weld_edges(meshes.iter().flat_map(|m| {
                let o = Vec3::from(m.origin);
                m.indices.as_chunks::<3>().0.iter().map(move |t| t.map(|i| Vec3::from(m.positions[i as usize]) + o))
            }));
            let pinched = edges.values().filter(|n| **n > 2).count();
            assert_eq!(pinched, 0, "stride {stride}: {pinched} edges shared by more than two triangles");
            assert!(
                against * 2000 < tris,
                "stride {stride}: {against} of {tris} triangles ({:.3}%) face against their \
                 vertex normals (limit 0.05%)",
                against as f64 * 100.0 / tris as f64
            );
        }
    }

    /// **A passage is capped at a coarser chunk wherever it crosses the face, not
    /// only where a lattice corner happens to land in it.** The cap test looked at the
    /// coarse face's corners, so a passage that crossed between them was shut by the
    /// minimum with nothing drawn: from inside it you looked through the rock, in a
    /// third to two thirds of placements. Round passages at 24 random offsets each,
    /// across a change of stride from 1 to 2, 2 to 4 and 4 to 8.
    #[test]
    fn a_passage_anywhere_across_a_coarser_face_is_capped() {
        let voxel = 1.5;
        let mut r = floptle_core::noise::Rng::new(24);
        let (mut windows, mut rays) = (Vec::new(), 0);
        for (fine, coarse, width) in [(1, 2, 1.5f32), (2, 4, 2.4), (2, 4, 3.0), (4, 8, 6.0)] {
            let cell = coarse as f32 * voxel;
            for _ in 0..24 {
                let c = Vec3::new(r.range(-1.0, 1.0) as f32 * cell, r.range(-1.0, 1.0) as f32 * cell, 0.0);
                let radius = width * 0.5;
                let mut f = ChunkField::new(voxel);
                // Rock well past the coarse cells around the passage, as inside a
                // planet: a cap needs rock round it on the face.
                f.fill_with(
                    Vec3::new(-44.0, -44.0, -46.0),
                    Vec3::new(44.0, 44.0, 46.0),
                    |p| {
                        let block = (p.abs() - Vec3::new(40.0, 40.0, 40.0)).max_element();
                        let tube = (Vec3::new(p.x, p.y, 0.0) - c).length() - radius;
                        block.max(-tube.max(p.z.abs() - 36.0))
                    },
                    |_| [0.5, 0.5, 0.5],
                );
                // The face between chunk z = -1 and chunk z = 0 is at z = 0.
                let meshes: Vec<ChunkMesh> = f
                    .chunk_coords()
                    .into_iter()
                    .map(|k| mesh_chunk(&f, k, if k[2] < 0 { fine } else { coarse }, true))
                    .collect();
                for (dx, dy) in [(0.0, 0.0), (0.4, 0.0), (-0.4, 0.0), (0.0, 0.4), (0.0, -0.4)] {
                    let o = c + Vec3::new(dx * radius, dy * radius, -12.0);
                    // From inside the passage the field holds: one that falls between
                    // the voxels is not in the field at all.
                    if f.d(o) <= 0.0 {
                        continue;
                    }
                    rays += 1;
                    let to = first_triangle(&meshes, o, Vec3::Z).map_or(f32::INFINITY, |t| o.z + t);
                    if to > cell + 0.5 * voxel {
                        windows.push(format!("{width} m, stride {fine} to {coarse}, passage at ({:.2}, {:.2}): a ray ran to z = {to:.1}", c.x, c.y));
                        break;
                    }
                }
            }
        }
        assert!(windows.is_empty(), "{} placements see through the rock: {windows:#?}", windows.len());
        assert!(rays > 300, "only {rays} rays started inside a passage");
    }

    /// **A chunk outside what an edit reached meshes exactly as it did.** The
    /// collider re-meshes only the chunks `chunks_reading` names after a dig; one
    /// it leaves out that the dig did change is ground a body stands on that is
    /// not there. Digs at random spots of a cavernous planet, near chunk faces
    /// and corners as much as anywhere, and every other chunk meshes the same.
    #[test]
    fn an_edit_changes_no_mesh_outside_the_chunks_reading_it() {
        let radius = 60.0;
        let mut f = crate::procgen::generate_planet(&crate::procgen::PlanetFill {
            seed: 3,
            radius,
            voxel: 1.0,
            relief: 4.0,
            cave_depth: 18.0,
            core_r: 4.0,
            ..Default::default()
        });
        let _ = f.take_changed();
        let mut r = floptle_core::noise::Rng::new(9);
        for _ in 0..12 {
            let dir = Vec3::new(r.range(-1.0, 1.0) as f32, r.range(-1.0, 1.0) as f32, r.range(-1.0, 1.0) as f32)
                .normalize_or_zero();
            let Some(ground) = f.raycast(dir * radius * 1.5, -dir, radius) else { continue };
            let before: Vec<([i32; 3], ChunkMesh)> =
                f.chunk_coords().into_iter().map(|c| (c, mesh_chunk(&f, c, 1, false))).collect();
            f.sculpt(Brush::Lower, ground, 2.0 + r.next_f64() as f32 * 2.0, 1.0, BrushProfile::default());
            let (lo, hi) = f.take_changed().expect("the dig wrote");
            let reading: std::collections::HashSet<[i32; 3]> = f.chunks_reading(lo, hi, 1).into_iter().collect();
            for (c, m) in before {
                if reading.contains(&c) {
                    continue;
                }
                let now = mesh_chunk(&f, c, 1, false);
                assert!(
                    now.positions == m.positions && now.indices == m.indices && now.normals == m.normals,
                    "chunk {c:?} changed under a dig at {ground:?} that reached only {lo:?}..{hi:?}"
                );
            }
        }
    }

    /// **Coarse ground stays near the real ground on a rough moon.** Each coarse
    /// vertex settled back onto the ground only where the ground faced within
    /// about 25° of the coarse cell, so on steep, curved ground most stayed
    /// where the solid-keeping filter pushed them: faces rising metres over the
    /// hills, which shrank into small hills as you walked up. Settling now
    /// takes ground within 60°, and the fold check undoes any it should not.
    /// Measured down 1,500 directions on a cratered moon with 15 m of relief.
    ///
    /// A stride-8 cell is 12 m across, wider than the moon's smaller bumps, and
    /// its minimum fills any hollow narrower than that; a vertex over one looked
    /// only a cell down for ground and missed the floor, and one whose settling
    /// folded a sliver went straight back up. Together they drew bowls as peaks
    /// standing over their rims: 18 m over the ground here at worst and 7 m at
    /// the 99th percentile, 20 m on a larger world. Settling now looks two cells
    /// down and a fold is eased out toward the vertices around it before
    /// anything goes back up. The craters here are steep 15 m pits two stride-8
    /// cells across, which is what the worst case still is.
    #[test]
    fn coarse_ground_stays_near_the_ground_on_a_rough_moon() {
        let f = crate::procgen::generate_planet(&crate::procgen::PlanetFill {
            seed: 248913517,
            radius: 99.345,
            voxel: 1.5,
            relief: 14.52,
            bump_freq: 4.241,
            cave_depth: 24.14,
            craters: 4,
            ..Default::default()
        });
        // What this measures today (see `mesh_chunk`): p99 0.63 / 1.43 / 4.12 m
        // and worst 1.32 / 4.10 / 12.2 m at strides 2 / 4 / 8.
        for (stride, p99, worst) in [(2, 0.75f32, 1.6f32), (4, 1.65, 4.7), (8, 4.75, 13.0)] {
            let hulls: Vec<Hull> = f.chunk_coords().into_iter().map(|c| hull(&mesh_chunk(&f, c, stride, false))).collect();
            let mut above = Vec::new();
            for d in directions(1500) {
                let ro = d * 160.0;
                let Some(truth) = f.raycast(ro, -d, 160.0) else { continue };
                let Some(t) = first_hit(&hulls, ro, -d, 320.0) else { continue };
                above.push((truth - ro).length() - t);
            }
            above.sort_by(f32::total_cmp);
            let at = above[((above.len() - 1) as f64 * 0.99) as usize];
            let top = above[above.len() - 1];
            assert!(at <= p99, "stride {stride}: the 99th percentile of coarse ground stands {at:.2} m over the real ground (limit {p99})");
            assert!(top <= worst, "stride {stride}: coarse ground stands {top:.2} m over the real ground at worst (limit {worst})");
        }
    }

    /// **Coarse ground does not fold on a cavernous planet.** Settling a coarse
    /// vertex onto ground its neighbours did not settle onto folded the triangles
    /// between them into shards facing against their normals: on this world, the
    /// Solar planet the caves were tuned on, 0.051% at stride 2 and 0.062% at
    /// stride 4, over the 0.05% the field-following guard allows.
    #[test]
    fn a_cavernous_planet_meshes_without_folds_at_every_stride() {
        let radius = 200.19;
        let f = crate::procgen::generate_planet(&crate::procgen::PlanetFill {
            seed: 800921623,
            radius,
            voxel: 1.5,
            relief: 13.11,
            bump_freq: 4.5,
            cave_depth: radius * 0.35,
            core_r: radius * 0.07,
            ..Default::default()
        });
        for stride in [2, 4, 8] {
            let (mut tris, mut against) = (0usize, 0usize);
            for c in f.chunk_coords() {
                let m = mesh_chunk(&f, c, stride, false);
                for t in m.indices.as_chunks::<3>().0 {
                    let p = t.map(|i| Vec3::from(m.positions[i as usize]));
                    let n: Vec3 = t.iter().map(|i| Vec3::from(m.normals[*i as usize])).sum();
                    tris += 1;
                    if (p[1] - p[0]).cross(p[2] - p[0]).normalize_or_zero().dot(n.normalize_or_zero()) < 0.0 {
                        against += 1;
                    }
                }
            }
            assert!(
                against * 2000 < tris,
                "stride {stride}: {against} of {tris} triangles ({:.3}%) face against their vertex normals (limit 0.05%)",
                against as f64 * 100.0 / tris as f64
            );
        }
    }

    /// The nearest triangle a ray from `o` along `d` meets, front or back.
    fn first_triangle(meshes: &[ChunkMesh], o: Vec3, d: Vec3) -> Option<f32> {
        let mut best: Option<f32> = None;
        for m in meshes {
            let org = Vec3::from(m.origin);
            for t in m.indices.as_chunks::<3>().0 {
                let [a, b, c] = t.map(|i| Vec3::from(m.positions[i as usize]) + org);
                let (e1, e2) = (b - a, c - a);
                let p = d.cross(e2);
                let det = e1.dot(p);
                if det.abs() < 1e-9 {
                    continue;
                }
                let s = o - a;
                let u = s.dot(p) / det;
                let q = s.cross(e1);
                let v = d.dot(q) / det;
                let t = e2.dot(q) / det;
                if u >= 0.0 && v >= 0.0 && u + v <= 1.0 && t > 1e-4 && best.is_none_or(|b| t < b) {
                    best = Some(t);
                }
            }
        }
        best
    }

    /// **A passage narrower than a coarse cell is capped where it meets a coarser
    /// chunk.** The coarse chunk's minimum shuts it at the shared face, the finer
    /// chunk's tunnel stops there, and the face belonged to the finer side, which
    /// sees it open. Neither drew it, and from inside the passage you looked into
    /// the rock. Rays down the passage now meet a surface within a coarse cell of
    /// the face, at stride 1 against 2 and at 2 against 4.
    #[test]
    fn a_narrow_passage_into_a_coarser_chunk_is_capped_at_the_face() {
        for (fine, coarse, half_width) in [(1, 2, 0.75f32), (2, 4, 1.5)] {
            let mut f = ChunkField::new(1.0);
            f.fill_with(
                Vec3::new(-14.0, -14.0, -40.0),
                Vec3::new(14.0, 14.0, 40.0),
                |p| {
                    let block = (p.abs() - Vec3::new(10.0, 10.0, 34.0)).max_element();
                    let tube = (Vec3::new(p.x, p.y, 0.0).abs() - Vec3::new(half_width, half_width, 0.0)).max_element();
                    block.max(-tube.max(p.z.abs() - 30.0))
                },
                |_| [0.5, 0.5, 0.5],
            );
            // The face between chunk z = -1 and chunk z = 0 is at z = 0.
            let meshes: Vec<ChunkMesh> = f
                .chunk_coords()
                .into_iter()
                .map(|c| mesh_chunk(&f, c, if c[2] < 0 { fine } else { coarse }, true))
                .collect();
            let mut worst = 0.0f32;
            for (dx, dy) in [(0.0, 0.0), (0.2, 0.1), (-0.2, 0.15), (0.1, -0.2)] {
                let o = Vec3::new(dx * half_width, dy * half_width, -12.0);
                let to = first_triangle(&meshes, o, Vec3::Z).map_or(f32::INFINITY, |t| o.z + t);
                worst = worst.max(to);
            }
            assert!(
                worst <= coarse as f32 + 0.5,
                "stride {fine} against {coarse}: a ray down the passage went to z = {worst} before meeting anything"
            );
        }
    }
}
