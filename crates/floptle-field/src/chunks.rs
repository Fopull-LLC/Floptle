//! Sparse chunked SDF — the authoring/physics/shadow substrate for Terrain 2.0
//! (`docs/subsystems/deformable-matter.md` §3.1).
//!
//! # Why this exists
//!
//! The dense [`crate::Terrain`] grid is O(n³) in the world's *volume*, but the surface —
//! the only part anyone sees or sculpts — is O(n²). Measured on a real project field:
//! 289×271×307 voxels = 24.4M = **192 MB**, with a hard 384-cell/axis cap that put a
//! ~576-unit ceiling on any map, and an undo step that snapshotted all 192 MB per
//! stroke. This module stores only the **narrow band** around the surface, in 32³
//! chunks, keyed sparsely — so the same map is single-digit MB and has no bounds at all.
//!
//! # The two invariants everything downstream leans on
//!
//! 1. **1-Lipschitz.** |∇d| ≤ 1: `d` never grows faster than distance itself can. Sphere
//!    tracing, gradient normals, SDF AO and sun shadows are all *wrong* without it — the
//!    dense field's `grow()` broke this by summing two distance terms (measured |∇d| up
//!    to 12) and the symptom was blotchy AO and speckle, not a crash. Every write path
//!    here is gated by [`ChunkField::assert_lipschitz`] in tests.
//! 2. **Border transparency.** Readers address *global voxel indices* and never know
//!    chunks exist ([`ChunkField::voxel_at`]). The mesher's aprons, gradients at chunk
//!    faces, and seam agreement all fall out of this for free (trap T3).
//!
//! # Band clamping
//!
//! Stored distances are clamped to ±`band` (= 4 voxels). Beyond the band the exact value
//! is irrelevant — nothing marches primary rays through this field any more — so regions
//! that are entirely air or entirely solid collapse to a [`Chunk::Uniform`] sentinel and
//! cost one enum. Clamping is monotone and cannot *increase* any gradient, so invariant
//! (1) survives it.

use std::collections::HashMap;

use floptle_core::math::Vec3;

use crate::chunk_data::ChunkData;
use crate::terrain::{Brush, BrushProfile};

/// Exact signed distance to an axis-aligned box: negative inside, positive outside,
/// |∇d| = 1 everywhere. The standard formulation.
fn box_sdf(p: Vec3, center: Vec3, half: Vec3) -> f32 {
    let q = (p - center).abs() - half;
    let outside = q.max(Vec3::ZERO).length();
    let inside = q.x.max(q.y).max(q.z).min(0.0);
    outside + inside
}

/// Voxels per chunk edge. 32³ ≈ 48 world units at the default 1.5-unit voxel: a good
/// brush-dirty granularity and a sub-millisecond remesh unit. 16³ doubles per-chunk
/// overhead; 64³ makes remesh and LOD rings too coarse.
pub const CHUNK: i32 = 32;
pub(crate) const CHUNK_U: usize = CHUNK as usize;
pub(crate) const CHUNK_VOXELS: usize = CHUNK_U * CHUNK_U * CHUNK_U;

/// Narrow-band half-width, in voxels. Distances are stored clamped to ±(band × voxel).
/// 4 gives the mesher and gradient stencils plenty of room either side of the surface.
pub const BAND_VOXELS: f32 = 4.0;

/// How many voxels past the band a flatten fades out over, along the plane's
/// normal. Nothing further from the plane than band + this is touched.
pub const FLATTEN_FALLOFF_VOXELS: f32 = 2.0;

/// A chunk's contents. Absent from the map == `Uniform(+band)` (open air).
#[derive(Clone, Debug)]
enum Chunk {
    /// Every voxel holds this exact value — the whole point of the sparse store. `+band`
    /// is sky, `-band` is deep interior; a hollow mountain costs two enums.
    Uniform(f32),
    /// Shared between copies of a field until one of them writes it: the play
    /// session's snapshot of a field and the physics sim's copy of it hold the
    /// same chunks as the field itself, not three of everything.
    Data(std::sync::Arc<ChunkData>),
    /// A chunk put away to save memory: the generator makes it again exactly
    /// when something comes near ([`ChunkField::evict`]), and until then this
    /// coarse lattice of it stands in for whoever reads it from afar.
    Lattice(std::sync::Arc<Lattice>),
}

/// Voxels between the points of a [`Lattice`].
const LATTICE_STRIDE: usize = 4;
/// Points along each edge of a [`Lattice`]: the chunk's corners included, its
/// far faces being its neighbours' first voxels.
const LATTICE_N: usize = CHUNK_U / LATTICE_STRIDE + 1;

/// An evicted chunk's stand-in: its distances and colours every
/// [`LATTICE_STRIDE`] voxels, read back trilinearly. About 6 KB against the
/// tens or hundreds of KB the chunk holds: what the coarsest meshes, a far
/// body's collision and a ray from orbit need of ground no one is near.
#[derive(Clone, Debug)]
pub(crate) struct Lattice {
    dist: Vec<f32>,
    color: Vec<[u8; 4]>,
}

impl Lattice {
    fn new(dist: Vec<f32>, color: Vec<[u8; 4]>) -> Self {
        Lattice { dist, color }
    }

    #[inline]
    fn at(x: usize, y: usize, z: usize) -> usize {
        (z * LATTICE_N + y) * LATTICE_N + x
    }

    /// The distance at voxel `li` of the chunk, trilinear between the points.
    #[inline]
    fn dist(&self, li: usize) -> f32 {
        let (x, y, z) = (li % CHUNK_U, (li / CHUNK_U) % CHUNK_U, li / (CHUNK_U * CHUNK_U));
        let (gx, gy, gz) = (x / LATTICE_STRIDE, y / LATTICE_STRIDE, z / LATTICE_STRIDE);
        let s = LATTICE_STRIDE as f32;
        let (fx, fy, fz) = ((x % LATTICE_STRIDE) as f32 / s, (y % LATTICE_STRIDE) as f32 / s, (z % LATTICE_STRIDE) as f32 / s);
        let d = |dx: usize, dy: usize, dz: usize| self.dist[Self::at(gx + dx, gy + dy, gz + dz)];
        let l = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let x0 = l(l(d(0, 0, 0), d(1, 0, 0), fx), l(d(0, 1, 0), d(1, 1, 0), fx), fy);
        let x1 = l(l(d(0, 0, 1), d(1, 0, 1), fx), l(d(0, 1, 1), d(1, 1, 1), fx), fy);
        l(x0, x1, fz)
    }

    /// The colour at voxel `li`: that of the corner of its cell nearest the
    /// ground, above it before below. The points are [`LATTICE_STRIDE`]
    /// voxels apart, so the nearest one to a voxel on the surface can be
    /// metres down, in the subsoil or the strata: a world seen from orbit
    /// showed grey squares where its chunks were put away. A point in the air
    /// is painted as the surface it is over.
    #[inline]
    fn color(&self, li: usize) -> [u8; 4] {
        let (x, y, z) = (li % CHUNK_U, (li / CHUNK_U) % CHUNK_U, li / (CHUNK_U * CHUNK_U));
        let (gx, gy, gz) = (x / LATTICE_STRIDE, y / LATTICE_STRIDE, z / LATTICE_STRIDE);
        let mut best = (f32::MAX, [0u8; 4]);
        for k in 0..8 {
            let i = Self::at(gx + (k & 1), gy + ((k >> 1) & 1), gz + (k >> 2));
            let d = self.dist[i];
            let key = if d >= 0.0 { d } else { 1e6 - d };
            if key < best.0 {
                best = (key, self.color[i]);
            }
        }
        best.1
    }

    fn heap_bytes(&self) -> usize {
        self.dist.len() * 4 + self.color.len() * 4
    }
}


/// A box of a dense proxy's cells sampled from a field, to be written into the
/// proxy later: the costly half of a proxy refresh, which can run where the
/// field is, off the frame. See [`ChunkField::sample_dense_region`].
#[derive(Clone, Debug)]
pub struct DenseRegion {
    /// The inclusive index box.
    mn: [u32; 3],
    mx: [u32; 3],
    layers: Vec<Vec<(f32, [u8; 4])>>,
}

impl DenseRegion {
    /// Write the cells into `baked`, returning the index box as
    /// [`ChunkField::refresh_dense_region`] does (inclusive min, exclusive max).
    pub fn write_into(&self, baked: &mut crate::mesh2sdf::BakedSdf) -> ([u32; 3], [u32; 3]) {
        let [w, h, _] = baked.dims;
        let (mn, mx) = (self.mn, self.mx);
        for (k, layer) in self.layers.iter().enumerate() {
            let iz = mn[2] + k as u32;
            let mut cells = layer.iter();
            for iy in mn[1]..=mx[1] {
                for ix in mn[0]..=mx[0] {
                    let i = ((iz * h + iy) * w + ix) as usize;
                    if let Some((d, c)) = cells.next() {
                        baked.distance[i] = *d;
                        baked.color[i] = *c;
                    }
                }
            }
        }
        (mn, [mx[0] + 1, mx[1] + 1, mx[2] + 1])
    }
}

/// Chunk coordinate → the chunk containing global voxel `i`.
#[inline]
fn chunk_of(i: [i32; 3]) -> [i32; 3] {
    [i[0].div_euclid(CHUNK), i[1].div_euclid(CHUNK), i[2].div_euclid(CHUNK)]
}

/// Index within a chunk for global voxel `i`.
#[inline]
fn local_of(i: [i32; 3]) -> usize {
    let x = i[0].rem_euclid(CHUNK) as usize;
    let y = i[1].rem_euclid(CHUNK) as usize;
    let z = i[2].rem_euclid(CHUNK) as usize;
    (z * CHUNK_U + y) * CHUNK_U + x
}

/// A sparse, unbounded, narrow-band signed distance field on a cubic voxel lattice.
///
/// Voxel `i` sits at world position `i * voxel` (lattice points, not cell centres — so
/// trilinear sampling between voxels is exact and chunk borders line up by construction).
#[derive(Clone, Debug)]
pub struct ChunkField {
    chunks: HashMap<[i32; 3], Chunk>,
    voxel: f32,
    /// Default colour for voxels in chunks that have never been painted.
    base_color: [u8; 4],
    /// The box of voxels written since [`Self::take_changed`] last asked.
    changed: Option<([i32; 3], [i32; 3])>,
    /// Chunks stored as deep solid that are really not generated yet: what
    /// they hold is buried rock and caves nothing can see until something
    /// comes near, and [`Self::realize`] makes them from `source`.
    pending: std::collections::HashSet<[i32; 3]>,
    source: Option<Source>,
    /// Was this field generated (from a planet's spec or any other source)?
    /// Only then do its unedited chunks mean anything: they are what the
    /// generator makes, and need not be kept or saved.
    generated: bool,
    /// The chunks written since the field was generated: by an edit, or by
    /// the projection an edit runs spilling into a neighbour. Everything else
    /// is what the generator made, voxel for voxel.
    edited: std::collections::HashSet<[i32; 3]>,
    /// Writes are the generator's own while this is set, and mark nothing.
    quiet: bool,
    /// Read from a file holding only the edited chunks: every other chunk is
    /// the generator's, and the field is not whole until it is laid over a
    /// fresh generation ([`Self::lay_over`]).
    delta: bool,
    /// What it was generated from, as its maker wrote it down: saved with the
    /// edits, so a save is laid back over the ground it was dug into even if
    /// the recipe has changed since.
    spec: Option<String>,
    /// Found its edits by [`Self::adopt_generation`]: what is on disk is the
    /// whole field, and the next save should replace it with the edits.
    converted: bool,
}

/// What makes a field's pending chunks: see [`ChunkField::generate_deferring`].
pub trait PendingSource: Send + Sync {
    /// The chunk at `c`, as the generator that deferred it would have made it.
    fn generate(&self, c: [i32; 3]) -> Generated;
}

/// One chunk [`ChunkField::evict`] put away: what it was and what stands in
/// for it, for the field's copies to follow ([`ChunkField::apply_evictions`]).
#[derive(Clone)]
pub struct Eviction {
    c: [i32; 3],
    old: Chunk,
    new: Chunk,
}

impl Eviction {
    /// The chunk put away.
    pub fn coord(&self) -> [i32; 3] {
        self.c
    }
}

/// A chunk a [`PendingSource`] made, ready for [`ChunkField::adopt_generated`].
pub struct Generated(Option<Chunk>);

/// A field's pending source, shareable with a worker thread.
#[derive(Clone)]
struct Source(std::sync::Arc<dyn PendingSource>);

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PendingSource")
    }
}

/// A [`VoxelSource`] that makes chunks on demand, with the frame of the fill
/// that deferred them.
struct Deferred<S> {
    src: std::sync::Arc<S>,
    ilo: [i32; 3],
    ihi: [i32; 3],
    voxel: f32,
    band: f32,
    base: [u8; 4],
    /// Did the fill leave the chunks `src` defers for later?
    defer: bool,
}

/// Voxels around a chunk its exact remaking samples: a voxel's value after
/// the fill's six sweeps depends on nothing further away than six.
const EXACT_APRON: i32 = 7;

impl<S: VoxelSource + Send + Sync + 'static> PendingSource for Deferred<S> {
    /// A chunk the fill deferred, made on its own (below). Any other chunk,
    /// made exactly as the fill made it ([`Self::exact`]): an evicted chunk
    /// comes back as it went.
    fn generate(&self, c: [i32; 3]) -> Generated {
        if self.deferred(c) {
            return self.alone(c);
        }
        self.exact(c)
    }
}

impl<S: VoxelSource + Send + Sync + 'static> Deferred<S> {
    /// Did the fill defer chunk `c`?
    fn deferred(&self, c: [i32; 3]) -> bool {
        let at = |i: [i32; 3]| Vec3::new(i[0] as f32, i[1] as f32, i[2] as f32) * self.voxel;
        let (s, e) = ([c[0] * CHUNK, c[1] * CHUNK, c[2] * CHUNK], [c[0] * CHUNK + CHUNK - 1, c[1] * CHUNK + CHUNK - 1, c[2] * CHUNK + CHUNK - 1]);
        self.defer && self.src.defer(at(s), at(e), self.band)
    }

    /// Chunk `c` exactly as the fill made it, without making its neighbours.
    ///
    /// The fill samples every chunk, then evens the whole field out with six
    /// sweeps in which each voxel reads only the six beside it as they were
    /// before the sweep. After six, a voxel's value depends on nothing further
    /// than six voxels from it. So the chunk and a seven-voxel apron are
    /// sampled as the fill sampled them (deferred neighbours as the solid
    /// stand-in the fill held for them, nothing past the fill's box), the same
    /// six sweeps run over that, and the chunk's own voxels come out as the
    /// fill's, bit for bit. The apron's outer layers come out wrong, and are
    /// thrown away.
    fn exact(&self, c: [i32; 3]) -> Generated {
        let band = self.band;
        let h = self.voxel;
        let Some(chunk) = gen_chunk(c, self.ilo, self.ihi, self.voxel, band, self.base, &*self.src) else {
            return Generated(None);
        };
        let Chunk::Data(data) = chunk else { return Generated(Some(chunk)) };
        let a = EXACT_APRON;
        let dim = (CHUNK + 2 * a) as usize;
        let lo = [c[0] * CHUNK - a, c[1] * CHUNK - a, c[2] * CHUNK - a];
        let mut d = vec![band; dim * dim * dim];
        let idx = |x: usize, y: usize, z: usize| (z * dim + y) * dim + x;
        let at = |i: [i32; 3]| Vec3::new(i[0] as f32, i[1] as f32, i[2] as f32) * self.voxel;
        let mut scratch = S::Scratch::default();
        // The fill's raw samples over the box, a classified block at a time.
        for nz in -1..=1 {
            for ny in -1..=1 {
                for nx in -1..=1 {
                    let nc = [c[0] + nx, c[1] + ny, c[2] + nz];
                    // The part of the box in this neighbour, in global voxels.
                    let s = [0, 1, 2].map(|k| (nc[k] * CHUNK).max(lo[k]).max(self.ilo[k]));
                    let e = [0, 1, 2].map(|k| (nc[k] * CHUNK + CHUNK - 1).min(lo[k] + dim as i32 - 1).min(self.ihi[k]));
                    if (0..3).any(|k| s[k] > e[k]) {
                        continue;
                    }
                    if nc == c {
                        for z in s[2]..=e[2] {
                            for y in s[1]..=e[1] {
                                for x in s[0]..=e[0] {
                                    d[idx((x - lo[0]) as usize, (y - lo[1]) as usize, (z - lo[2]) as usize)] = data.dist(local_of([x, y, z]));
                                }
                            }
                        }
                        continue;
                    }
                    if self.deferred(nc) {
                        for z in s[2]..=e[2] {
                            for y in s[1]..=e[1] {
                                for x in s[0]..=e[0] {
                                    d[idx((x - lo[0]) as usize, (y - lo[1]) as usize, (z - lo[2]) as usize)] = -band;
                                }
                            }
                        }
                        continue;
                    }
                    // Classified in blocks, as the fill does: a block it can
                    // place is the value it places, and the rest is sampled.
                    let mut bz = s[2];
                    while bz <= e[2] {
                        let ez = (bz.div_euclid(BLOCK) * BLOCK + BLOCK - 1).min(e[2]);
                        let mut by = s[1];
                        while by <= e[1] {
                            let ey = (by.div_euclid(BLOCK) * BLOCK + BLOCK - 1).min(e[1]);
                            let mut bx = s[0];
                            while bx <= e[0] {
                                let ex = (bx.div_euclid(BLOCK) * BLOCK + BLOCK - 1).min(e[0]);
                                let class = self.src.classify(at([bx, by, bz]), at([ex, ey, ez]), band);
                                for z in bz..=ez {
                                    for y in by..=ey {
                                        for x in bx..=ex {
                                            let v = match class {
                                                BlockClass::Air => band,
                                                BlockClass::Solid => -band,
                                                BlockClass::Mixed => self
                                                    .src
                                                    .sample(at([x, y, z]), band, &mut scratch)
                                                    .map_or(band, |(v, _)| v.clamp(-band, band)),
                                            };
                                            d[idx((x - lo[0]) as usize, (y - lo[1]) as usize, (z - lo[2]) as usize)] = v;
                                        }
                                    }
                                }
                                bx = ex + 1;
                            }
                            by = ey + 1;
                        }
                        bz = ez + 1;
                    }
                }
            }
        }
        // The fill's six sweeps, as `renormalize` runs them.
        for _ in 0..6 {
            let mut writes: Vec<(usize, f32)> = Vec::new();
            for z in 0..dim {
                for y in 0..dim {
                    for x in 0..dim {
                        let i = idx(x, y, z);
                        let cur = d[i];
                        if cur.abs() >= band {
                            continue;
                        }
                        let (mut lo_b, mut hi_b) = (f32::NEG_INFINITY, f32::INFINITY);
                        let mut nb = |n: f32| {
                            if n.abs() < band {
                                lo_b = lo_b.max(n - h);
                                hi_b = hi_b.min(n + h);
                            }
                        };
                        if x > 0 {
                            nb(d[i - 1]);
                        }
                        if x + 1 < dim {
                            nb(d[i + 1]);
                        }
                        if y > 0 {
                            nb(d[i - dim]);
                        }
                        if y + 1 < dim {
                            nb(d[i + dim]);
                        }
                        if z > 0 {
                            nb(d[i - dim * dim]);
                        }
                        if z + 1 < dim {
                            nb(d[i + dim * dim]);
                        }
                        if !lo_b.is_finite() || !hi_b.is_finite() {
                            continue;
                        }
                        let next = if lo_b > hi_b { 0.5 * (lo_b + hi_b) } else { cur.clamp(lo_b, hi_b) };
                        if (next - cur).abs() > 1e-6 {
                            writes.push((i, next));
                        }
                    }
                }
            }
            if writes.is_empty() {
                break;
            }
            for (i, v) in writes {
                d[i] = v.clamp(-band, band);
            }
        }
        let mut out = (*data).clone();
        for z in 0..CHUNK {
            for y in 0..CHUNK {
                for x in 0..CHUNK {
                    let li = ((z * CHUNK + y) * CHUNK + x) as usize;
                    out.set_dist(li, d[idx((x + a) as usize, (y + a) as usize, (z + a) as usize)]);
                }
            }
        }
        // As `compact` leaves it.
        match out.uniform_dist() {
            Some(v) if v.abs() >= band - 1e-6 => Generated(Some(Chunk::Uniform(v))),
            _ => {
                out.shrink();
                Generated(Some(Chunk::Data(std::sync::Arc::new(out))))
            }
        }
    }

    /// The chunk, evened out on its own, as if nothing were around it: so what
    /// comes back depends on where it is and nothing else, and a chunk made
    /// before its neighbours is the same as one made after them. Two players
    /// whose games made a planet's caves in different orders have the same
    /// caves.
    fn alone(&self, c: [i32; 3]) -> Generated {
        let Some(chunk) = gen_chunk(c, self.ilo, self.ihi, self.voxel, self.band, self.base, &*self.src) else {
            return Generated(None);
        };
        let mut alone = ChunkField::new(self.voxel);
        alone.base_color = self.base;
        alone.chunks.insert(c, chunk);
        alone.renormalize(&[c]);
        alone.compact(&[c]);
        Generated(alone.chunks.remove(&c))
    }
}

impl Default for ChunkField {
    /// An empty unit-voxel field — a valid placeholder before a real field is derived.
    fn default() -> Self {
        Self::new(1.0)
    }
}

/// In-progress incremental `.cfield` serialization — see [`ChunkField::begin_save`].
/// Holds the chunk worklist and the growing blob; the field it came from does the
/// encoding ([`ChunkField::save_step`]), a budget of chunks per call.
pub struct FieldSaver {
    /// Writing only the edited chunks; one edited down to air is written too,
    /// or the generator's rock would come back on load.
    delta: bool,
    spec: Option<String>,
    coords: Vec<[i32; 3]>,
    at: usize,
    written: u32,
    count_pos: usize,
    buf: Vec<u8>,
    pending: Vec<[i32; 3]>,
}

impl FieldSaver {
    /// Chunks still waiting to encode.
    pub fn remaining(&self) -> usize {
        self.coords.len() - self.at
    }

    /// Total chunks in the worklist (progress display: `1 - remaining/total`).
    pub fn total(&self) -> usize {
        self.coords.len()
    }

    /// Take the finished blob (call once [`ChunkField::save_step`] returns `true`).
    pub fn finish(mut self) -> Vec<u8> {
        self.buf[self.count_pos..self.count_pos + 4]
            .copy_from_slice(&self.written.to_le_bytes());
        // The recipe a file of edits is laid back over. Only such a file has
        // one, and only a reader that knows version 2 reads such a file.
        if let Some(spec) = &self.spec {
            self.buf.extend_from_slice(b"SPEC");
            self.buf.extend_from_slice(&(spec.len() as u32).to_le_bytes());
            self.buf.extend_from_slice(spec.as_bytes());
        }
        // Which of the solid chunks are not generated yet, after the chunks: a
        // reader that does not know the section stops before it and reads them
        // as the rock they stand in for.
        if !self.pending.is_empty() {
            self.buf.extend_from_slice(b"PEND");
            self.buf.extend_from_slice(&(self.pending.len() as u32).to_le_bytes());
            for c in &self.pending {
                for k in c {
                    self.buf.extend_from_slice(&k.to_le_bytes());
                }
            }
        }
        self.buf
    }
}

impl ChunkField {
    /// An empty field of open air. `voxel` is the cubic voxel edge in world units — the
    /// one density knob (the dense grid's "detail" was a cell *count* that silently
    /// meant nothing as terrain grew; this is a real density).
    pub fn new(voxel: f32) -> Self {
        Self {
            chunks: HashMap::new(),
            voxel: voxel.max(1e-3),
            base_color: [128, 128, 128, 255],
            changed: None,
            pending: std::collections::HashSet::new(),
            source: None,
            generated: false,
            edited: std::collections::HashSet::new(),
            quiet: false,
            delta: false,
            spec: None,
            converted: false,
        }
    }

    pub fn voxel(&self) -> f32 {
        self.voxel
    }

    /// The narrow-band half-width in world units. Stored distances saturate here.
    pub fn band(&self) -> f32 {
        BAND_VOXELS * self.voxel
    }

    pub fn set_base_color(&mut self, c: [f32; 3]) {
        self.base_color = [
            (c[0].clamp(0.0, 1.0) * 255.0).round() as u8,
            (c[1].clamp(0.0, 1.0) * 255.0).round() as u8,
            (c[2].clamp(0.0, 1.0) * 255.0).round() as u8,
            255,
        ];
    }

    /// Number of chunks holding real voxel data (Uniform sentinels are free).
    pub fn data_chunks(&self) -> usize {
        self.chunks.values().filter(|c| matches!(c, Chunk::Data(_) | Chunk::Lattice(_))).count()
    }

    /// Resident bytes of voxel data — what the 192 MB dense field is measured against.
    pub fn memory_bytes(&self) -> usize {
        self.chunks
            .values()
            .map(|c| match c {
                Chunk::Data(d) => d.heap_bytes(),
                Chunk::Lattice(l) => l.heap_bytes(),
                Chunk::Uniform(_) => 0,
            })
            .sum()
    }

    /// Was this field generated, so that its unedited chunks are the generator's?
    pub fn is_generated(&self) -> bool {
        self.generated
    }

    /// Has chunk `c` been written since the field was generated? Always true
    /// of every chunk of a field that was not.
    pub fn is_edited(&self, c: [i32; 3]) -> bool {
        !self.generated || self.edited.contains(&c)
    }

    /// How many chunks have been written since the field was generated.
    pub fn edited_count(&self) -> usize {
        self.edited.len()
    }

    /// Record what this field was generated from. A generated field with a
    /// recipe saves only its edits and the recipe ([`Self::begin_save`]).
    pub fn set_spec(&mut self, spec: String) {
        self.spec = Some(spec);
    }

    /// Did this field find its edits in a whole-field save
    /// ([`Self::adopt_generation`])? Its file is then worth writing again
    /// even with nothing new dug: the edits alone replace the whole field.
    pub fn was_converted(&self) -> bool {
        self.converted
    }

    /// Forget the recipe: the field saves whole again.
    pub fn clear_spec(&mut self) {
        self.spec = None;
    }

    /// What this field was generated from, if its maker said.
    pub fn spec(&self) -> Option<&str> {
        self.spec.as_deref()
    }

    /// Was this read from a file holding only the edits? Such a field is laid
    /// over a fresh generation from its [`Self::spec`] with [`Self::lay_over`]
    /// before it is used.
    pub fn is_delta(&self) -> bool {
        self.delta
    }

    /// Lay these edits over `base`, the field generated from their spec: the
    /// edited chunks replace the generator's, and every other chunk stays the
    /// generator's. The result saves as edits again.
    pub fn lay_over(self, mut base: ChunkField) -> ChunkField {
        let band = base.band();
        for (c, ch) in self.chunks {
            base.pending.remove(&c);
            match ch {
                Chunk::Uniform(v) if v >= band - 1e-6 => {
                    base.chunks.remove(&c);
                }
                ch => {
                    base.chunks.insert(c, ch);
                }
            }
        }
        base.edited = self.edited;
        base.generated = true;
        base.spec = self.spec;
        base.changed = None;
        base
    }

    /// Find which chunks of this field, read from a whole-field save, differ
    /// from `base`, the field its spec generates, and take the generator's
    /// chunk for every one that does not: it is the same ground, held exactly
    /// rather than as the file rounded it. From then on the field saves only
    /// its edits. A save from before edits were tracked holds no record of
    /// them, so this is how one becomes small.
    pub fn adopt_generation(&mut self, base: &ChunkField) {
        let band = self.band();
        let q = |v: f32| ((v / band) * 127.0).round().clamp(-127.0, 127.0) as i8;
        let dist = |ch: Option<&Chunk>, li: usize| match ch {
            None => band,
            Some(Chunk::Uniform(v)) => *v,
            Some(Chunk::Data(d)) => d.dist(li),
            Some(Chunk::Lattice(l)) => l.dist(li),
        };
        let color = |ch: Option<&Chunk>, li: usize, base_color: [u8; 4]| match ch {
            Some(Chunk::Data(d)) => d.color(li),
            Some(Chunk::Lattice(l)) => l.color(li),
            _ => base_color,
        };
        let mut coords: Vec<[i32; 3]> = self.chunks.keys().chain(base.chunks.keys()).copied().collect();
        coords.sort_unstable();
        coords.dedup();
        let source = base.pending_source();
        // On every core: a big planet is thousands of chunks of 32,768 voxels.
        let differs = par_map(coords.len(), |k| {
            let c = coords[k];
            // Never made in this field: the generator's, whatever it is.
            if self.pending.contains(&c) {
                return false;
            }
            // Made since by a dig or a visit: compare with what the generator
            // makes of it now.
            let made;
            let theirs = if base.pending.contains(&c) {
                match &source {
                    Some(src) => {
                        made = src.generate(c).0;
                        made.as_ref()
                    }
                    None => return true,
                }
            } else {
                base.chunks.get(&c)
            };
            let ours = self.chunks.get(&c);
            !(0..CHUNK_VOXELS).all(|li| {
                q(dist(ours, li)) == q(dist(theirs, li))
                    && (dist(ours, li) >= band - 1e-6 || color(ours, li, self.base_color) == color(theirs, li, base.base_color))
            })
        });
        let edited: std::collections::HashSet<[i32; 3]> =
            coords.iter().zip(differs).filter(|(_, d)| *d).map(|(c, _)| *c).collect();
        // The generator's field, with this one's differing chunks laid over it.
        let mut out = base.clone();
        for &c in &edited {
            out.pending.remove(&c);
            match self.chunks.get(&c) {
                Some(ch) => {
                    out.chunks.insert(c, ch.clone());
                }
                None => {
                    out.chunks.remove(&c);
                }
            }
        }
        out.edited = edited;
        out.generated = true;
        out.spec = self.spec.take().or_else(|| base.spec.clone());
        out.converted = true;
        *self = out;
    }

    /// Every stored chunk coordinate — data and uniform sentinels. The undo-snapshot
    /// set for whole-field ops (Fill), where any stored chunk may change.
    pub fn all_chunk_coords(&self) -> Vec<[i32; 3]> {
        self.chunks.keys().copied().collect()
    }

    /// Every chunk coordinate overlapping a world-space AABB (present or absent) —
    /// the undo-snapshot set for ops that may create chunks inside a known box.
    pub fn chunks_in_world_box(&self, min: Vec3, max: Vec3) -> Vec<[i32; 3]> {
        let (c0, c1) = (
            chunk_of([
                (min.x / self.voxel).floor() as i32,
                (min.y / self.voxel).floor() as i32,
                (min.z / self.voxel).floor() as i32,
            ]),
            chunk_of([
                (max.x / self.voxel).ceil() as i32,
                (max.y / self.voxel).ceil() as i32,
                (max.z / self.voxel).ceil() as i32,
            ]),
        );
        let mut out = Vec::new();
        for cz in c0[2]..=c1[2] {
            for cy in c0[1]..=c1[1] {
                for cx in c0[0]..=c1[0] {
                    out.push([cx, cy, cz]);
                }
            }
        }
        out
    }

    /// Every chunk coordinate holding data, for meshing/persistence.
    pub fn chunk_coords(&self) -> Vec<[i32; 3]> {
        self.chunks
            .iter()
            .filter(|(_, c)| matches!(c, Chunk::Data(_) | Chunk::Lattice(_)))
            .map(|(k, _)| *k)
            .collect()
    }

    /// Whether a chunk holds data — the chunks `chunk_coords` lists and a mesher meshes.
    pub fn has_data(&self, c: [i32; 3]) -> bool {
        matches!(self.chunks.get(&c), Some(Chunk::Data(_) | Chunk::Lattice(_)))
    }

    /// World position of a chunk's `(0,0,0)` voxel — the origin its mesh is relative to.
    pub fn chunk_origin(&self, c: [i32; 3]) -> Vec3 {
        Vec3::new(
            c[0] as f32 * CHUNK as f32 * self.voxel,
            c[1] as f32 * CHUNK as f32 * self.voxel,
            c[2] as f32 * CHUNK as f32 * self.voxel,
        )
    }

    // ---- sampling: border-transparent, chunks are invisible to readers -------

    /// The texture-palette slot at a world position — the material the
    /// generator wrote when it laid down strata, ore pockets and cave seams.
    ///
    /// `None` for untextured voxels (slot 0, and the legacy 255 sentinel), so a
    /// caller can tell "plain rock" from "slot 255". The read half of what
    /// `paint_texture` writes, so a mining game can ask what it has just dug.
    pub fn slot_at(&self, p: Vec3) -> Option<u8> {
        match self.color(p)[3] {
            0 | 255 => None,
            s => Some(s),
        }
    }

    /// The stored distance at a global voxel index. Absent chunks read as open air.
    /// This is the only voxel accessor; aprons and cross-chunk gradients come free (T3).
    #[inline]
    pub fn voxel_at(&self, i: [i32; 3]) -> f32 {
        match self.chunks.get(&chunk_of(i)) {
            None => self.band(),
            Some(Chunk::Uniform(v)) => *v,
            Some(Chunk::Data(d)) => d.dist(local_of(i)),
            Some(Chunk::Lattice(l)) => l.dist(local_of(i)),
        }
    }

    #[inline]
    fn color_at(&self, i: [i32; 3]) -> [u8; 4] {
        match self.chunks.get(&chunk_of(i)) {
            Some(Chunk::Data(d)) => d.color(local_of(i)),
            Some(Chunk::Lattice(l)) => l.color(local_of(i)),
            _ => self.base_color,
        }
    }

    /// Whether [`Self::d`] reads at least the band at every point of the box
    /// `min ..= max`: every voxel a read anywhere in it can touch holds a
    /// distance from the band to twice the band, which only open air does.
    ///
    /// Judged from what the chunks store: an absent chunk is open air, a
    /// uniform one is its value, and a stored one answers by its blocks, one
    /// holding a single value by that value and one holding varied values by
    /// the voxels the box covers. A coarse stand-in for an evicted chunk says
    /// no. So `false` means "cannot tell", never "solid", and a box wider than
    /// a few chunks is not asked about.
    ///
    /// Why the values must be at most twice the band: `d` blends neighbours,
    /// and a blend of two values that close is never rounded below the
    /// smaller of them, so every read lands at or above the band.
    pub fn open_air_in_box(&self, min: Vec3, max: Vec3) -> bool {
        if !(min.is_finite() && max.is_finite()) {
            return false;
        }
        let band = self.band();
        let (lo, hi) = (min / self.voxel, max / self.voxel);
        let lo = [lo.x.floor() as i32, lo.y.floor() as i32, lo.z.floor() as i32];
        // A read uses the voxel at the floor and the one past it.
        let hi = [hi.x.floor() as i32 + 1, hi.y.floor() as i32 + 1, hi.z.floor() as i32 + 1];
        let (c0, c1) = (chunk_of(lo), chunk_of(hi));
        if (0..3).any(|k| c1[k] - c0[k] > 3) {
            return false;
        }
        for cz in c0[2]..=c1[2] {
            for cy in c0[1]..=c1[1] {
                for cx in c0[0]..=c1[0] {
                    let ok = match self.chunks.get(&[cx, cy, cz]) {
                        None => true,
                        Some(Chunk::Uniform(v)) => *v >= band && *v <= 2.0 * band,
                        Some(Chunk::Lattice(_)) => false,
                        Some(Chunk::Data(d)) => {
                            let at = [cx, cy, cz];
                            let span = |k: usize| {
                                let base = at[k] * CHUNK;
                                ((lo[k].max(base) - base) as usize, (hi[k].min(base + CHUNK - 1) - base) as usize)
                            };
                            let (x, y, z) = (span(0), span(1), span(2));
                            d.fills_within([x.0, y.0, z.0], [x.1, y.1, z.1], band, 2.0 * band)
                        }
                    };
                    if !ok {
                        return false;
                    }
                }
            }
        }
        true
    }

    /// Trilinearly-sampled distance at a world position.
    pub fn d(&self, p: Vec3) -> f32 {
        let g = p / self.voxel;
        let b = [g.x.floor() as i32, g.y.floor() as i32, g.z.floor() as i32];
        let f = Vec3::new(g.x - b[0] as f32, g.y - b[1] as f32, g.z - b[2] as f32);
        // All eight corners in one chunk, which is all but the last voxel of
        // each row: that chunk is found once rather than once a corner. The
        // same voxels are read, so the same value comes back.
        let one_chunk = b.iter().all(|&k| k.rem_euclid(CHUNK) < CHUNK - 1);
        let chunk = if one_chunk { Some(self.chunks.get(&chunk_of(b))) } else { None };
        let c = |dx: i32, dy: i32, dz: i32| {
            let i = [b[0] + dx, b[1] + dy, b[2] + dz];
            match chunk {
                Some(None) => self.band(),
                Some(Some(Chunk::Uniform(v))) => *v,
                Some(Some(Chunk::Data(d))) => d.dist(local_of(i)),
                Some(Some(Chunk::Lattice(l))) => l.dist(local_of(i)),
                None => self.voxel_at(i),
            }
        };
        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let x00 = lerp(c(0, 0, 0), c(1, 0, 0), f.x);
        let x10 = lerp(c(0, 1, 0), c(1, 1, 0), f.x);
        let x01 = lerp(c(0, 0, 1), c(1, 0, 1), f.x);
        let x11 = lerp(c(0, 1, 1), c(1, 1, 1), f.x);
        lerp(lerp(x00, x10, f.y), lerp(x01, x11, f.y), f.z)
    }

    /// Trilinearly-sampled colour at a world position.
    pub fn color(&self, p: Vec3) -> [u8; 4] {
        let g = p / self.voxel;
        let b = [g.x.round() as i32, g.y.round() as i32, g.z.round() as i32];
        self.color_at(b)
    }

    /// Field gradient (the surface normal direction) at a world position, by central
    /// differences one voxel wide.
    ///
    /// This is the reason terrain stops looking faceted: on the GPU the normal was
    /// re-derived per pixel from a trilinearly-filtered f16 texture, and trilinear
    /// interpolation is only C⁰ — its gradient jumps at every cell face, so the lattice
    /// showed through the shading no matter how cubic the voxels were. Here the gradient
    /// is computed once per vertex in f32 and then interpolated across the triangle by
    /// the rasterizer, exactly like an imported mesh's normals.
    pub fn grad(&self, p: Vec3) -> Vec3 {
        let h = self.voxel;
        let g = Vec3::new(
            self.d(p + Vec3::X * h) - self.d(p - Vec3::X * h),
            self.d(p + Vec3::Y * h) - self.d(p - Vec3::Y * h),
            self.d(p + Vec3::Z * h) - self.d(p - Vec3::Z * h),
        );
        g.normalize_or_zero()
    }

    // ---- writes -------------------------------------------------------------

    /// Global voxel index range (inclusive) touched by a brush at `center`/`radius`.
    fn voxel_range(&self, center: Vec3, radius: f32) -> ([i32; 3], [i32; 3]) {
        // One voxel of slop so the band around the brush is written too.
        let r = radius + self.voxel * (BAND_VOXELS + 1.0);
        let lo = (center - Vec3::splat(r)) / self.voxel;
        let hi = (center + Vec3::splat(r)) / self.voxel;
        (
            [lo.x.floor() as i32, lo.y.floor() as i32, lo.z.floor() as i32],
            [hi.x.ceil() as i32, hi.y.ceil() as i32, hi.z.ceil() as i32],
        )
    }

    /// Make a chunk writable, materializing a `Uniform` sentinel into real voxels.
    fn data_mut(&mut self, c: [i32; 3]) -> &mut ChunkData {
        // A write into a chunk not generated yet generates it first, or the
        // write would land on stand-in rock and the chunk be made over it later.
        if self.pending.contains(&c) {
            self.realize_now(&[c]);
        }
        if !self.quiet {
            self.edited.insert(c);
        }
        let band = BAND_VOXELS * self.voxel;
        let base = self.base_color;
        let entry = self.chunks.entry(c).or_insert(Chunk::Uniform(band));
        match entry {
            Chunk::Uniform(v) => *entry = Chunk::Data(std::sync::Arc::new(ChunkData::new(*v, base))),
            // A stand-in with nothing to make it again from: what it says is
            // all there is of the chunk now.
            Chunk::Lattice(l) => {
                let mut d = ChunkData::new(0.0, base);
                for li in 0..CHUNK_VOXELS {
                    d.set_dist(li, l.dist(li));
                    d.set_color(li, l.color(li));
                }
                *entry = Chunk::Data(std::sync::Arc::new(d));
            }
            Chunk::Data(_) => {}
        }
        match entry {
            Chunk::Data(d) => std::sync::Arc::make_mut(d),
            _ => unreachable!("just materialized"),
        }
    }

    /// Widen the changed box to take in voxel `i`.
    #[inline]
    fn note_changed(&mut self, i: [i32; 3]) {
        self.changed = Some(match self.changed {
            None => (i, i),
            Some((lo, hi)) => (
                [lo[0].min(i[0]), lo[1].min(i[1]), lo[2].min(i[2])],
                [hi[0].max(i[0]), hi[1].max(i[1]), hi[2].max(i[2])],
            ),
        });
    }

    /// The box, in voxel indices, of every voxel written since the last call,
    /// and start a new one. `None` when nothing was.
    ///
    /// Finer than the chunks an edit reports: a dig touches a chunk or two but
    /// writes a few voxels in them, and whatever keeps meshes of the field (a
    /// collider's triangles) needs only re-mesh the chunks whose meshes read
    /// this box, rather than every neighbour of every chunk touched.
    pub fn take_changed(&mut self) -> Option<([i32; 3], [i32; 3])> {
        self.changed.take()
    }

    /// Every chunk whose unskirted mesh at `stride` can change with a voxel of
    /// the box `lo..=hi` (see [`crate::mesh_depends`]).
    pub fn chunks_reading(&self, lo: [i32; 3], hi: [i32; 3], stride: i32) -> Vec<[i32; 3]> {
        let m = crate::mesh_depends(stride);
        let (c0, c1) = (chunk_of([lo[0] - m, lo[1] - m, lo[2] - m]), chunk_of([hi[0] + m, hi[1] + m, hi[2] + m]));
        let mut out = Vec::new();
        for z in c0[2]..=c1[2] {
            for y in c0[1]..=c1[1] {
                for x in c0[0]..=c1[0] {
                    out.push([x, y, z]);
                }
            }
        }
        out
    }

    /// Every chunk holding a voxel of the box `lo..=hi`.
    pub fn chunks_holding(&self, lo: [i32; 3], hi: [i32; 3]) -> Vec<[i32; 3]> {
        let (c0, c1) = (chunk_of(lo), chunk_of(hi));
        let mut out = Vec::new();
        for z in c0[2]..=c1[2] {
            for y in c0[1]..=c1[1] {
                for x in c0[0]..=c1[0] {
                    out.push([x, y, z]);
                }
            }
        }
        out
    }

    #[inline]
    fn set_voxel(&mut self, i: [i32; 3], d: f32, color: Option<[u8; 4]>) {
        self.note_changed(i);
        let band = BAND_VOXELS * self.voxel;
        let li = local_of(i);
        let data = self.data_mut(chunk_of(i));
        data.set_dist(li, d.clamp(-band, band));
        if let Some(c) = color {
            data.set_color(li, c);
        }
    }

    /// [`Self::renormalize`] for a brush that knows which voxels it wrote: the
    /// same projection, visiting only what the write could have disturbed.
    ///
    /// The first sweep looks at the written voxels and their six neighbours; each
    /// later sweep only at the neighbours of what the previous one changed. A voxel
    /// the edit could not reach keeps the value a whole-chunk sweep would have left
    /// it, so the result is the same, and the cost follows the size of the edit
    /// rather than the chunks around it: a flatten on a planet swept every chunk it
    /// grazed and a ring of their neighbours, six times, at seven hash lookups a
    /// voxel, which was most of a second per building pad.
    ///
    /// Only voxels `may_write` accepts are changed; the rest are read but kept. A
    /// brush that must not reach past its own extent says so here, because the
    /// projection otherwise carries an edit on, sweep by sweep, into rock the
    /// brush itself left alone.
    ///
    /// Returns the chunks it changed, which can spill one voxel past the write.
    fn renormalize_from(
        &mut self,
        mut active: Vec<[i32; 3]>,
        may_write: impl Fn([i32; 3]) -> bool,
    ) -> Vec<[i32; 3]> {
        const NEIGHBOURS: [(i32, i32, i32); 6] =
            [(-1, 0, 0), (1, 0, 0), (0, -1, 0), (0, 1, 0), (0, 0, -1), (0, 0, 1)];
        let h = self.voxel;
        let band = self.band();
        let mut changed: Vec<[i32; 3]> = Vec::new();
        let grow = |set: &mut Vec<[i32; 3]>| {
            let n = set.len();
            for k in 0..n {
                let i = set[k];
                for (dx, dy, dz) in NEIGHBOURS {
                    set.push([i[0] + dx, i[1] + dy, i[2] + dz]);
                }
            }
            // Chunk-major order, so the reader's one-chunk cache hits.
            set.sort_unstable_by_key(|i| (chunk_of(*i), *i));
            set.dedup();
        };
        grow(&mut active);
        for _ in 0..6 {
            let mut writes: Vec<([i32; 3], f32)> = Vec::new();
            {
                let mut read = VoxelReader::new(self);
                for &i in &active {
                    if !may_write(i) {
                        continue;
                    }
                    let cur = read.at(i);
                    if cur.abs() >= band {
                        continue; // saturated plateau: zero gradient by design
                    }
                    let (mut lo, mut hi) = (f32::NEG_INFINITY, f32::INFINITY);
                    for (dx, dy, dz) in NEIGHBOURS {
                        let n = read.at([i[0] + dx, i[1] + dy, i[2] + dz]);
                        if n.abs() >= band {
                            continue;
                        }
                        lo = lo.max(n - h);
                        hi = hi.min(n + h);
                    }
                    if !lo.is_finite() || !hi.is_finite() {
                        continue;
                    }
                    let next = if lo > hi { 0.5 * (lo + hi) } else { cur.clamp(lo, hi) };
                    if (next - cur).abs() > 1e-6 {
                        writes.push((i, next));
                    }
                }
            }
            if writes.is_empty() {
                break;
            }
            // Jacobi, as in `renormalize`: every read above saw the same field.
            active.clear();
            for (i, v) in writes {
                let li = local_of(i);
                let c = chunk_of(i);
                self.note_changed(i);
                let data = self.data_mut(c);
                data.set_dist(li, v.clamp(-band, band));
                changed.push(c);
                active.push(i);
            }
            grow(&mut active);
        }
        changed.sort_unstable();
        changed.dedup();
        changed
    }

    /// Project the field back onto the 1-Lipschitz constraint over the given chunks.
    ///
    /// **Why this is not optional.** A brush moves each voxel by its own weight, so a
    /// falloff (or a hard edge) leaves neighbouring voxels differing by more than one
    /// voxel of distance — |∇d| climbs above 1 and the field stops being a distance
    /// field. Measured on the first cut of these brushes: 5.4% of band voxels bad, worst
    /// 4.36. That is the same failure the dense `grow()` shipped (11.1% / 12.00), whose
    /// symptom was blotchy AO and speckle rather than an obvious crash.
    ///
    /// The projection is the standard one: a distance field obeys
    /// `|d(a)| ≤ |d(b)| + h` for neighbours `a`,`b` one voxel `h` apart. Clamping each
    /// magnitude down to its neighbours' minimum + h, a few sweeps, converges to a
    /// 1-Lipschitz field. It only ever reduces |d| and never changes a sign, so the zero
    /// crossing — the surface the mesher extracts — stays put.
    fn renormalize(&mut self, coords: &[[i32; 3]]) {
        let h = self.voxel;
        let band = self.band();
        // Include one ring of neighbours: a dab at a chunk's edge constrains the voxels
        // just across the border too.
        let mut region: Vec<[i32; 3]> = Vec::new();
        for c in coords {
            for dz in -1..=1 {
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        region.push([c[0] + dx, c[1] + dy, c[2] + dz]);
                    }
                }
            }
        }
        region.sort_unstable();
        region.dedup();
        region.retain(|c| matches!(self.chunks.get(c), Some(Chunk::Data(_))));

        // The constraint is on the signed field: adjacent voxels one `h` apart can
        // differ by at most `h`, which is exactly |∇d| ≤ 1. (Constraining |d| instead
        // is a weaker, different condition and barely moved the needle: 5.4% → 4.9%.)
        // Clamping each voxel into the interval its neighbours allow is the standard
        // alternating projection; a few sweeps converge, and a dab only perturbs the
        // field a voxel or two from where it wrote.
        //
        // Each chunk's sweep reads only its own voxels and the one-voxel apron
        // around them, gathered into a flat block so the stencil is array
        // indexing rather than seven hash lookups a voxel, and the chunks are
        // independent within a sweep, so they share out across threads.
        const DIM: usize = CHUNK_U + 2;
        for _ in 0..6 {
            let sweep = par_map(region.len(), |k| {
                let c = region[k];
                let lo = [c[0] * CHUNK - 1, c[1] * CHUNK - 1, c[2] * CHUNK - 1];
                let (mut dist, mut color) = (Vec::new(), Vec::new());
                self.gather(lo, DIM, &mut dist, &mut color);
                let at = |x: usize, y: usize, z: usize| dist[(z * DIM + y) * DIM + x];
                let mut writes: Vec<(usize, f32)> = Vec::new();
                for lz in 0..CHUNK_U {
                    for ly in 0..CHUNK_U {
                        for lx in 0..CHUNK_U {
                            let (x, y, z) = (lx + 1, ly + 1, lz + 1);
                            let cur = at(x, y, z);
                            if cur.abs() >= band {
                                continue; // saturated plateau: zero gradient by design
                            }
                            let (mut lo, mut hi) = (f32::NEG_INFINITY, f32::INFINITY);
                            for n in [
                                at(x - 1, y, z),
                                at(x + 1, y, z),
                                at(x, y - 1, z),
                                at(x, y + 1, z),
                                at(x, y, z - 1),
                                at(x, y, z + 1),
                            ] {
                                // A saturated neighbour is a bound we don't trust — it
                                // says "at least band away", not "exactly band away".
                                if n.abs() >= band {
                                    continue;
                                }
                                lo = lo.max(n - h);
                                hi = hi.min(n + h);
                            }
                            if !lo.is_finite() || !hi.is_finite() {
                                continue;
                            }
                            let next = if lo > hi {
                                // Neighbours disagree by more than 2h between themselves;
                                // the midpoint is the least-wrong value and the next
                                // sweep tightens it.
                                0.5 * (lo + hi)
                            } else {
                                cur.clamp(lo, hi)
                            };
                            if (next - cur).abs() > 1e-6 {
                                writes.push(((lz * CHUNK_U + ly) * CHUNK_U + lx, next));
                            }
                        }
                    }
                }
                writes
            });
            if sweep.iter().all(Vec::is_empty) {
                break; // already 1-Lipschitz — the common case after a gentle dab
            }
            // Jacobi, not Gauss-Seidel: updating in place would make the result depend
            // on iteration order (and bias the surface along +x).
            for (c, writes) in region.iter().zip(sweep) {
                if writes.is_empty() {
                    continue;
                }
                for &(li, _) in &writes {
                    let (x, y, z) = (li % CHUNK_U, (li / CHUNK_U) % CHUNK_U, li / (CHUNK_U * CHUNK_U));
                    self.note_changed([c[0] * CHUNK + x as i32, c[1] * CHUNK + y as i32, c[2] * CHUNK + z as i32]);
                }
                let data = self.data_mut(*c);
                for (li, v) in writes {
                    data.set_dist(li, v.clamp(-band, band));
                }
            }
        }
    }

    /// Collapse chunks whose voxels are all one value back to a sentinel — this is what
    /// keeps a large map's memory proportional to its surface rather than its volume.
    fn compact(&mut self, coords: &[[i32; 3]]) {
        let band = BAND_VOXELS * self.voxel;
        for c in coords {
            let uniform = match self.chunks.get_mut(c) {
                Some(Chunk::Data(d)) => {
                    // Only collapse fully-saturated chunks: an interior value would lose
                    // real information, a band-saturated one carries none.
                    match d.uniform_dist() {
                        Some(v) if v.abs() >= band - 1e-6 => Some(v),
                        _ => {
                            // What stays data is packed down to what it says.
                            // A chunk another copy still shares was packed when
                            // it was written.
                            if let Some(d) = std::sync::Arc::get_mut(d) {
                                d.shrink();
                            }
                            None
                        }
                    }
                }
                _ => None,
            };
            if let Some(v) = uniform {
                self.chunks.insert(*c, Chunk::Uniform(v));
            }
        }
    }

    /// Overwrite the given chunks with `src`'s (cloning present ones, removing
    /// absent ones) — the mirror primitive: after an edit lands on an authority
    /// field, copying the touched chunks into a collider's working copy makes the
    /// two agree *byte-for-byte* by construction, instead of trusting a re-run of
    /// the same op to converge (any drift there is a player standing on air).
    pub fn copy_chunks_from(&mut self, src: &ChunkField, coords: &[[i32; 3]]) {
        if self.source.is_none() {
            self.source = src.source.clone();
        }
        for c in coords {
            if src.pending.contains(c) {
                self.pending.insert(*c);
            } else {
                self.pending.remove(c);
            }
            match src.chunks.get(c) {
                Some(chunk) => {
                    self.chunks.insert(*c, chunk.clone());
                }
                None => {
                    self.chunks.remove(c);
                }
            }
        }
    }

    /// Take an edit made on a copy of this field: the chunks at `coords` as
    /// the copy has them, marked edited as the copy marks them, and the box of
    /// voxels the edit `changed` (what the copy's [`Self::take_changed`]
    /// gave). An edit too big for one frame is made on a copy off the frame
    /// and taken here whole, so nothing ever reads it half made.
    pub fn adopt_edit(&mut self, src: &ChunkField, coords: &[[i32; 3]], changed: Option<([i32; 3], [i32; 3])>) {
        self.copy_chunks_from(src, coords);
        for c in coords {
            if src.edited.contains(c) {
                self.edited.insert(*c);
            }
        }
        if let Some((lo, hi)) = changed {
            self.note_changed(lo);
            self.note_changed(hi);
        }
    }
}

/// How much material a sculpt moved, and what it was made of.
///
/// Volumes are in the field's own (local) cubic units; a caller that has scaled
/// its terrain node converts by the scale cubed. Additive across dabs: summing
/// `removed` over a shaft gives the volume that left the field.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SculptYield {
    /// Solid volume that left the field.
    pub removed: f64,
    /// Solid volume that arrived (a `Raise` dab).
    pub added: f64,
    /// `removed`, split by texture-palette slot.
    pub slots: std::collections::BTreeMap<u8, f64>,
    /// The part of `removed` that carried no palette slot — plain rock. Kept
    /// separate rather than folded into a slot so `removed` always equals
    /// `untextured + sum(slots)` and a caller can check its own arithmetic.
    pub untextured: f64,
}

/// The fraction of a voxel that is solid, from its signed distance: the linear
/// surface approximation the mesher already implies. Clamped, so voxels well
/// inside or well outside contribute a whole unit or nothing and the sum over a
/// region is the region's volume.
#[inline]
fn occupancy(d: f32, voxel: f32) -> f32 {
    (0.5 - d / voxel).clamp(0.0, 1.0)
}

/// What a generator can say about a block of voxels without evaluating them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockClass {
    /// Every voxel is at least a band out in open air.
    Air,
    /// Every voxel is at least a band deep in solid.
    Solid,
    /// Neither is certain: evaluate each voxel.
    Mixed,
}

/// A shape [`ChunkField::generate`] can build a field from.
///
/// `classify` must never be wrong in the direction of certainty: `Air` or
/// `Solid` for a block where any voxel is nearer the surface than the band
/// writes a wall the shape does not have. `Mixed` is always a safe answer.
pub trait VoxelSource: Sync {
    /// Working memory one thread keeps across the voxels it samples.
    type Scratch: Default;
    /// What is in the world-space box `[lo, hi]`, given the band half-width.
    fn classify(&self, lo: Vec3, hi: Vec3, band: f32) -> BlockClass;
    /// The distance at `p`, with its colour, or `None` when it is a band or
    /// more out in open air (where no colour is needed).
    fn sample(&self, p: Vec3, band: f32, scratch: &mut Self::Scratch) -> Option<(f32, [u8; 4])>;
    /// The colour at `p`, which `classify` placed in deep solid.
    fn solid_color(&self, p: Vec3, scratch: &mut Self::Scratch) -> [u8; 4];
    /// May the chunk in the box `[lo, hi]` be left for later: is it something
    /// nothing can see from outside until it comes near? Read only by
    /// [`ChunkField::generate_deferring`]. No, by default.
    fn defer(&self, lo: Vec3, hi: Vec3, band: f32) -> bool {
        let _ = (lo, hi, band);
        false
    }
}

/// Voxels per edge of the blocks [`ChunkField::generate`] classifies.
const BLOCK: i32 = 4;

/// One chunk of [`ChunkField::generate`]: `None` when it is all open air.
fn gen_chunk<S: VoxelSource>(
    c: [i32; 3],
    ilo: [i32; 3],
    ihi: [i32; 3],
    voxel: f32,
    band: f32,
    base: [u8; 4],
    src: &S,
) -> Option<Chunk> {
    let mut data: Option<ChunkData> = None;
    let fresh = || ChunkData::new(band, base);
    let at = |i: [i32; 3]| Vec3::new(i[0] as f32, i[1] as f32, i[2] as f32) * voxel;
    // The whole chunk first: most of a planet's cube is sky or buried rock.
    let b = [c[0] * CHUNK, c[1] * CHUNK, c[2] * CHUNK];
    let s = [b[0].max(ilo[0]), b[1].max(ilo[1]), b[2].max(ilo[2])];
    let e = [(b[0] + CHUNK - 1).min(ihi[0]), (b[1] + CHUNK - 1).min(ihi[1]), (b[2] + CHUNK - 1).min(ihi[2])];
    match src.classify(at(s), at(e), band) {
        BlockClass::Air => return None,
        // Deep solid is the sentinel `compact` would make of it anyway. (A
        // chunk cut by the edge of the fill keeps unwritten air in it.)
        BlockClass::Solid if s == b && e == [b[0] + CHUNK - 1, b[1] + CHUNK - 1, b[2] + CHUNK - 1] => {
            return Some(Chunk::Uniform(-band));
        }
        _ => {}
    }
    let (mut solid, mut wrote) = (0usize, 0usize);
    let mut scratch = S::Scratch::default();
    let blocks = CHUNK / BLOCK;
    for bz in 0..blocks {
        for by in 0..blocks {
            for bx in 0..blocks {
                let b = [c[0] * CHUNK + bx * BLOCK, c[1] * CHUNK + by * BLOCK, c[2] * CHUNK + bz * BLOCK];
                let s = [b[0].max(ilo[0]), b[1].max(ilo[1]), b[2].max(ilo[2])];
                let e = [
                    (b[0] + BLOCK - 1).min(ihi[0]),
                    (b[1] + BLOCK - 1).min(ihi[1]),
                    (b[2] + BLOCK - 1).min(ihi[2]),
                ];
                if s[0] > e[0] || s[1] > e[1] || s[2] > e[2] {
                    continue;
                }
                let class = src.classify(at(s), at(e), band);
                if class == BlockClass::Air {
                    continue;
                }
                let d = data.get_or_insert_with(fresh);
                for z in s[2]..=e[2] {
                    for y in s[1]..=e[1] {
                        for x in s[0]..=e[0] {
                            let li = local_of([x, y, z]);
                            let p = at([x, y, z]);
                            if class == BlockClass::Solid {
                                d.set_dist(li, -band);
                                d.set_color(li, src.solid_color(p, &mut scratch));
                                solid += 1;
                                wrote += 1;
                            } else if let Some((v, col)) = src.sample(p, band, &mut scratch) {
                                let v = v.clamp(-band, band);
                                d.set_dist(li, v);
                                d.set_color(li, col);
                                solid += usize::from(v == -band);
                                wrote += 1;
                            }
                        }
                    }
                }
            }
        }
    }
    // Wholly deep solid is the sentinel `compact` would make of it anyway.
    if solid == CHUNK_VOXELS {
        return Some(Chunk::Uniform(-band));
    }
    // A block that might have held ground but did not leaves no chunk, as an
    // unwritten one never existed.
    data.filter(|_| wrote > 0).map(|d| Chunk::Data(std::sync::Arc::new(d)))
}

/// How many threads a bulk field job shares out across: every core but one,
/// so the frame keeps a core while a planet generates behind it.
#[cfg(not(target_arch = "wasm32"))]
fn workers() -> usize {
    std::thread::available_parallelism().map(|n| n.get().saturating_sub(1)).unwrap_or(1).max(1)
}

/// `(0..n).map(f)`, shared out across [`workers`]; results in index order.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn par_map<T: Send>(n: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let threads = workers().min(n);
    if threads <= 1 {
        return (0..n).map(f).collect();
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut out: Vec<Option<T>> = (0..n).map(|_| None).collect();
    std::thread::scope(|s| {
        let job = || {
            let mut got = Vec::new();
            loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if i >= n {
                    break got;
                }
                got.push((i, f(i)));
            }
        };
        let handles: Vec<_> = (0..threads).map(|_| s.spawn(job)).collect();
        for h in handles {
            for (i, v) in h.join().expect("a field worker panicked") {
                out[i] = Some(v);
            }
        }
    });
    out.into_iter().map(|v| v.expect("every index ran")).collect()
}

/// The web has no threads to share out across: in order, on this one.
#[cfg(target_arch = "wasm32")]
pub(crate) fn par_map<T: Send>(n: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    (0..n).map(f).collect()
}

/// Reads voxels with the last chunk it looked up kept to hand: neighbouring
/// voxels are nearly always in the same chunk, so most reads skip the hash.
struct VoxelReader<'a> {
    field: &'a ChunkField,
    last: Option<([i32; 3], Option<&'a Chunk>)>,
}

impl<'a> VoxelReader<'a> {
    fn new(field: &'a ChunkField) -> Self {
        Self { field, last: None }
    }

    #[inline]
    fn at(&mut self, i: [i32; 3]) -> f32 {
        let c = chunk_of(i);
        let chunk = match self.last {
            Some((lc, ch)) if lc == c => ch,
            _ => {
                let ch = self.field.chunks.get(&c);
                self.last = Some((c, ch));
                ch
            }
        };
        match chunk {
            None => self.field.band(),
            Some(Chunk::Uniform(v)) => *v,
            Some(Chunk::Data(d)) => d.dist(local_of(i)),
            Some(Chunk::Lattice(l)) => l.dist(local_of(i)),
        }
    }
}

impl ChunkField {
    /// Apply a sculpt brush; returns the chunk coords whose voxels changed (the remesh
    /// set). Mirrors the dense [`crate::Terrain::sculpt`] semantics, including
    /// [`BrushProfile`], so sculpting feels identical.
    pub fn sculpt(
        &mut self,
        brush: Brush,
        center: Vec3,
        radius: f32,
        strength: f32,
        profile: BrushProfile,
    ) -> Vec<[i32; 3]> {
        self.sculpt_measured(brush, center, radius, strength, profile).0
    }

    /// [`Self::sculpt`], plus **how much material moved and what it was made of**.
    ///
    /// A dig that says nothing about what it removed leaves a mining game
    /// guessing: the palette slot each voxel carries is what the generator wrote
    /// when it laid down strata, ore pockets and cave seams, and it was
    /// write-only from script.
    ///
    /// Volume comes from each voxel's **occupancy** — the fraction of it that is
    /// solid, `clamp(0.5 - d/voxel, 0, 1)` — differenced across the write. That
    /// makes it additive by construction: sum the reports over a shaft and you
    /// get the volume that actually left the field, because every voxel is
    /// counted once and monotonically. Slots are read before the write, so the
    /// material reported is the material that was there.
    pub fn sculpt_measured(
        &mut self,
        brush: Brush,
        center: Vec3,
        radius: f32,
        strength: f32,
        profile: BrushProfile,
    ) -> (Vec<[i32; 3]>, SculptYield) {
        match brush {
            Brush::Paint => return (Vec::new(), SculptYield::default()),
            Brush::Smooth => {
                return (self.smooth(center, radius, strength, profile), SculptYield::default());
            }
            Brush::Flatten => {
                return (self.flatten(center, radius, strength, profile, Vec3::Y), SculptYield::default());
            }
            _ => {}
        }
        // CSG, not accumulation. The obvious brush — nudge every voxel toward
        // solid/air by its own weight — is what the dense field did, and it is not a
        // distance-field operation: after N dabs an inside voxel has moved N × strength
        // × voxel while its neighbour just outside the radius has moved 0, so |∇d|
        // explodes (measured 5.4% of band voxels bad, worst 4.36, and no amount of
        // post-hoc projection repairs a field the write path keeps breaking).
        //
        // Union/subtract an analytic ball instead. min/max — and smin/smax — of two
        // 1-Lipschitz fields is 1-Lipschitz, so the invariant holds by construction
        // rather than by repair. This is also how SDF sculpting is normally done, and it
        // makes a dab idempotent: holding the brush still does not dig to infinity.
        //
        //   strength  -> how much of the ball this dab deposits (its radius)
        //   hardness  -> the smin blend k: hard = a crisp ball, soft = a gentle swell
        let s = strength.clamp(0.02, 1.0);
        let r_eff = radius * s;
        let k = (1.0 - profile.hardness.clamp(0.0, 1.0)) * radius * 0.5;
        let (lo, hi) = self.voxel_range(center, radius);
        self.realize_voxels(lo, hi);
        let mut touched = Vec::new();
        let mut yielded = SculptYield::default();
        let vox_vol = f64::from(self.voxel).powi(3);
        for iz in lo[2]..=hi[2] {
            for iy in lo[1]..=hi[1] {
                for ix in lo[0]..=hi[0] {
                    let p = Vec3::new(ix as f32, iy as f32, iz as f32) * self.voxel;
                    // The ball's own SDF — exact, |∇d| = 1 everywhere.
                    let ball = (p - center).length() - r_eff;
                    let cur = self.voxel_at([ix, iy, iz]);
                    let next = match brush {
                        Brush::Raise => crate::smin(cur, ball, k),
                        // Subtract: intersect with the ball's complement (whose SDF is
                        // -ball): max(cur, -ball) = -min(-cur, ball). Getting the ball's
                        // sign wrong here computes max(cur, ball) instead — which keeps
                        // the ball and carves away everything else in the write box, a
                        // giant square crater per dab (the shipped bug the regression
                        // test below pins down).
                        Brush::Lower => -crate::smin(-cur, ball, k),
                        _ => cur,
                    };
                    if (next - cur).abs() < 1e-6 {
                        continue;
                    }
                    // Read the slot before the write; the material reported has
                    // to be the material that was standing there.
                    let d_occ = occupancy(cur, self.voxel) - occupancy(next, self.voxel);
                    let moved = f64::from(d_occ) * vox_vol;
                    if moved > 0.0 {
                        yielded.removed += moved;
                        let slot = self.color_at([ix, iy, iz])[3];
                        // 255 is the legacy "untextured" sentinel, not slot 255.
                        if slot != 0 && slot != 255 {
                            *yielded.slots.entry(slot).or_default() += moved;
                        } else {
                            yielded.untextured += moved;
                        }
                    } else {
                        yielded.added -= moved;
                    }
                    self.set_voxel([ix, iy, iz], next, None);
                    touched.push(chunk_of([ix, iy, iz]));
                }
            }
        }
        touched.sort_unstable();
        touched.dedup();
        self.compact(&touched);
        (touched, yielded)
    }

    /// Paint surface colour without touching the shape.
    pub fn paint(
        &mut self,
        center: Vec3,
        radius: f32,
        strength: f32,
        color: [f32; 3],
        profile: BrushProfile,
    ) -> Vec<[i32; 3]> {
        let rgb = [color[0] * 255.0, color[1] * 255.0, color[2] * 255.0];
        let s = strength.clamp(0.0, 1.0);
        let (lo, hi) = self.voxel_range(center, radius);
        self.realize_voxels(lo, hi);
        let band = self.band();
        let mut touched = Vec::new();
        for iz in lo[2]..=hi[2] {
            for iy in lo[1]..=hi[1] {
                for ix in lo[0]..=hi[0] {
                    let p = Vec3::new(ix as f32, iy as f32, iz as f32) * self.voxel;
                    let dc = (p - center).length();
                    if dc > radius {
                        continue;
                    }
                    // Only near the surface — painting deep interior tints nothing you
                    // could ever see, and would materialize solid chunks for no reason.
                    if self.voxel_at([ix, iy, iz]).abs() > band {
                        continue;
                    }
                    let w = s * profile.weight(dc, radius);
                    if w <= 0.0 {
                        continue;
                    }
                    let cur = self.color_at([ix, iy, iz]);
                    let mut out = cur;
                    for k in 0..3 {
                        out[k] = (cur[k] as f32 + (rgb[k] - cur[k] as f32) * w)
                            .round()
                            .clamp(0.0, 255.0) as u8;
                    }
                    let d = self.voxel_at([ix, iy, iz]);
                    self.set_voxel([ix, iy, iz], d, Some(out));
                    touched.push(chunk_of([ix, iy, iz]));
                }
            }
        }
        touched.sort_unstable();
        touched.dedup();
        touched
    }

    fn smooth(&mut self, center: Vec3, radius: f32, strength: f32, profile: BrushProfile) -> Vec<[i32; 3]> {
        let s = strength.clamp(0.0, 1.0);
        let (lo, hi) = self.voxel_range(center, radius);
        self.realize_voxels(lo, hi);
        // Read the pre-dab field for every sample: averaging against partially-updated
        // neighbours would let iteration order bias the result.
        let mut writes = Vec::new();
        for iz in lo[2]..=hi[2] {
            for iy in lo[1]..=hi[1] {
                for ix in lo[0]..=hi[0] {
                    let p = Vec3::new(ix as f32, iy as f32, iz as f32) * self.voxel;
                    let dc = (p - center).length();
                    if dc > radius {
                        continue;
                    }
                    let w = s * profile.weight(dc, radius);
                    if w <= 0.0 {
                        continue;
                    }
                    let mut acc = 0.0;
                    for (dx, dy, dz) in
                        [(-1, 0, 0), (1, 0, 0), (0, -1, 0), (0, 1, 0), (0, 0, -1), (0, 0, 1)]
                    {
                        acc += self.voxel_at([ix + dx, iy + dy, iz + dz]);
                    }
                    let avg = acc / 6.0;
                    let cur = self.voxel_at([ix, iy, iz]);
                    writes.push(([ix, iy, iz], cur + (avg - cur) * w));
                }
            }
        }
        let mut touched = Vec::new();
        let mut seeds = Vec::with_capacity(writes.len());
        for (i, v) in writes {
            if (v - self.voxel_at(i)).abs() < 1e-6 {
                continue;
            }
            self.set_voxel(i, v, None);
            touched.push(chunk_of(i));
            seeds.push(i);
        }
        touched.extend(self.renormalize_from(seeds, |_| true));
        touched.sort_unstable();
        touched.dedup();
        self.compact(&touched);
        touched
    }

    /// [`Brush::Flatten`] toward the plane through `center` facing `normal`: the
    /// footprint is a disc in that plane (distance measured perpendicular to the
    /// normal) and every voxel in it is pulled toward the plane's distance. The
    /// plain brush uses `+Y`; a planet passes its radial up at `center`, or the
    /// "flat" pad is tilted by the latitude.
    pub fn flatten_to_plane(
        &mut self,
        center: Vec3,
        normal: Vec3,
        radius: f32,
        strength: f32,
        profile: BrushProfile,
    ) -> Vec<[i32; 3]> {
        self.flatten(center, radius, strength, profile, normal)
    }

    fn flatten(
        &mut self,
        center: Vec3,
        radius: f32,
        strength: f32,
        profile: BrushProfile,
        normal: Vec3,
    ) -> Vec<[i32; 3]> {
        let n = normal.try_normalize().unwrap_or(Vec3::Y);
        let s = strength.clamp(0.0, 1.0);
        let band = self.band();
        let (lo, hi) = self.voxel_range(center, radius);
        self.realize_voxels(lo, hi);
        // Read everything first, then write: the reads share one chunk cache, and
        // a voxel whose value would not change is never written at all. Deep rock
        // under the pad and open sky above it are already as far from the plane as
        // the field can say, so writing them only turned compact chunks into dense
        // ones for the projection below to sweep.
        let mut writes: Vec<([i32; 3], f32)> = Vec::new();
        {
            let mut read = VoxelReader::new(self);
            for iz in lo[2]..=hi[2] {
                for iy in lo[1]..=hi[1] {
                    for ix in lo[0]..=hi[0] {
                        let p = Vec3::new(ix as f32, iy as f32, iz as f32) * self.voxel;
                        let rel = p - center;
                        let along = rel.dot(n);
                        let across = (rel - n * along).length();
                        if across > radius {
                            continue;
                        }
                        // Only near the plane. Rock or a cave further from it
                        // than the band is left as it was: the plane's own SDF
                        // is clamped to the band there, so pulling toward it
                        // filled a cave under a pad solid down to the edge of
                        // the brush's box, and left a flat wall where that edge
                        // cut the cave.
                        let reach = (along.abs() - band) / (FLATTEN_FALLOFF_VOXELS * self.voxel);
                        if reach >= 1.0 {
                            continue;
                        }
                        let w = s * profile.weight(across, radius) * (1.0 - reach.clamp(0.0, 1.0));
                        if w <= 0.0 {
                            continue;
                        }
                        // The plane's SDF through the hit point.
                        let cur = read.at([ix, iy, iz]);
                        let next = (cur + (along - cur) * w).clamp(-band, band);
                        if (next - cur).abs() < 1e-6 {
                            continue;
                        }
                        writes.push(([ix, iy, iz], next));
                    }
                }
            }
        }
        let mut touched = Vec::with_capacity(writes.len());
        let mut seeds = Vec::with_capacity(writes.len());
        for (i, v) in writes {
            self.set_voxel(i, v, None);
            touched.push(chunk_of(i));
            seeds.push(i);
        }
        // The projection keeps to the band and falloff the pad wrote within, plus
        // half a voxel. Left free it walked the edit down into a cave under the
        // pad, a voxel a sweep, and filled its roof with rock.
        let (voxel, limit) = (self.voxel, band + FLATTEN_FALLOFF_VOXELS * self.voxel + 0.5 * self.voxel);
        touched.extend(self.renormalize_from(seeds, |i| {
            let p = Vec3::new(i[0] as f32, i[1] as f32, i[2] as f32) * voxel;
            (p - center).dot(n).abs() <= limit
        }));
        touched.sort_unstable();
        touched.dedup();
        self.compact(&touched);
        touched
    }

    /// Lay a flat slab of solid ground: every voxel below `top_y` inside the X/Z box gets
    /// the plane distance, unioned with what's there. The starting point for a new
    /// terrain — no bounds needed, so "how big is my terrain" stops being a question you
    /// have to answer up front (and get wrong).
    pub fn fill_slab(&mut self, min: Vec3, max: Vec3, top_y: f32, color: [f32; 3]) {
        self.set_base_color(color);
        let c = [
            (color[0].clamp(0.0, 1.0) * 255.0).round() as u8,
            (color[1].clamp(0.0, 1.0) * 255.0).round() as u8,
            (color[2].clamp(0.0, 1.0) * 255.0).round() as u8,
            255,
        ];
        // Write the slab's BOX SDF, not a plane clipped to a region.
        //
        // The obvious version — `d = p.y - top_y` inside the box, air outside — makes
        // the field jump from -band (solid) to +band (air) across one voxel at the
        // slab's rim, because nothing ever wrote the ramp between them. Measured: worst
        // |∇d| = 4.36 on a bare slab, before any brush touched it. That is the same
        // mistake the dense `grow()` made (a cliff in the field rather than in the
        // geometry), and it is why sculpting looked broken near terrain edges.
        //
        // A box SDF is exact and 1-Lipschitz everywhere, including outside, so the rim
        // gets a proper distance ramp and the invariant holds from the first write.
        let top = Vec3::new(max.x, top_y, max.z);
        let bmin = Vec3::new(min.x, min.y, min.z);
        let bcenter = (bmin + top) * 0.5;
        let bhalf = (top - bmin) * 0.5;
        let band = self.band();
        // Extend past the box by the band so the outside ramp is actually stored.
        let lo = (bmin - Vec3::splat(band + self.voxel)) / self.voxel;
        let hi = (top + Vec3::splat(band + self.voxel)) / self.voxel;
        let mut touched = Vec::new();
        for iz in lo.z.floor() as i32..=hi.z.ceil() as i32 {
            for iy in lo.y.floor() as i32..=hi.y.ceil() as i32 {
                for ix in lo.x.floor() as i32..=hi.x.ceil() as i32 {
                    let p = Vec3::new(ix as f32, iy as f32, iz as f32) * self.voxel;
                    let d = box_sdf(p, bcenter, bhalf);
                    // Skip far AIR only — never far solid. Skipping deep solid leaves the
                    // interior at the chunk's materialization default (+band = air): a
                    // hollow shell with a phantom inner surface, exactly the bug
                    // `resample_dense` documents. Deep-solid writes are free after
                    // `compact` collapses saturated chunks to `Uniform(-band)` sentinels.
                    if d >= band {
                        continue; // outside, far: the air sentinel already says this
                    }
                    let cur = self.voxel_at([ix, iy, iz]);
                    self.set_voxel([ix, iy, iz], d.min(cur), Some(c));
                    touched.push(chunk_of([ix, iy, iz]));
                }
            }
        }
        // Deep interior is solid, not sky: mark those chunks so the volume is closed.
        let ilo = chunk_of([
            (bmin.x / self.voxel).floor() as i32,
            (bmin.y / self.voxel).floor() as i32,
            (bmin.z / self.voxel).floor() as i32,
        ]);
        let ihi = chunk_of([
            (top.x / self.voxel).ceil() as i32,
            (top.y / self.voxel).ceil() as i32,
            (top.z / self.voxel).ceil() as i32,
        ]);
        for cz in ilo[2]..=ihi[2] {
            for cy in ilo[1]..=ihi[1] {
                for cx in ilo[0]..=ihi[0] {
                    if self.chunks.contains_key(&[cx, cy, cz]) {
                        continue;
                    }
                    // A chunk wholly inside the slab and untouched by the band is solid.
                    let o = self.chunk_origin([cx, cy, cz]);
                    let mid = o + Vec3::splat(CHUNK as f32 * self.voxel * 0.5);
                    if box_sdf(mid, bcenter, bhalf) < -band {
                        self.chunks.insert([cx, cy, cz], Chunk::Uniform(-band));
                    }
                }
            }
        }
        touched.sort_unstable();
        touched.dedup();
        self.compact(&touched);
    }

    /// Write one voxel's distance directly, band-clamped. For building analytic
    /// reference fields (tests, tools, importers) — brushes go through [`Self::sculpt`].
    pub fn write_voxel(&mut self, i: [i32; 3], d: f32) {
        self.set_voxel(i, d, None);
    }

    /// Fill a region from an **analytic SDF** — the procedural-generation primitive
    /// (planetoids, cave systems, any shape math can describe). `sdf` is sampled at
    /// every voxel of `[min, max]` (padded by the band); values are unioned with
    /// what's there (`min`), colours come from `color` where the sdf wrote. Deep
    /// solid collapses to interior sentinels via `compact`, so a filled planet costs
    /// its surface, not its volume. The SDF should be ≈1-Lipschitz like every field
    /// write (bounded noise displacement on a distance is fine — the sphere-trace
    /// steps half-distances for exactly this reason).
    pub fn fill_with(
        &mut self,
        min: Vec3,
        max: Vec3,
        sdf: impl Fn(Vec3) -> f32,
        color: impl Fn(Vec3) -> [f32; 3],
    ) {
        // Alpha 255 is the legacy "no slot" sentinel (untextured), not slot 255 —
        // generators that want textured voxels use `fill_with_rgba` and author the
        // palette slot in the alpha byte themselves.
        self.fill_with_rgba(min, max, sdf, |p| {
            let c = color(p);
            [
                (c[0].clamp(0.0, 1.0) * 255.0).round() as u8,
                (c[1].clamp(0.0, 1.0) * 255.0).round() as u8,
                (c[2].clamp(0.0, 1.0) * 255.0).round() as u8,
                255,
            ]
        });
    }

    /// [`Self::fill_with`] with full control of the voxel color: RGB tint bytes plus
    /// the alpha byte, which is the 1-based terrain palette slot (0 and 255 both mean
    /// untextured). This is how procedural generators author *materials* — strata,
    /// biomes, glowing cave veins — not just tints.
    pub fn fill_with_rgba(
        &mut self,
        min: Vec3,
        max: Vec3,
        sdf: impl Fn(Vec3) -> f32,
        color: impl Fn(Vec3) -> [u8; 4],
    ) {
        let band = self.band();
        let lo = (min - Vec3::splat(band + self.voxel)) / self.voxel;
        let hi = (max + Vec3::splat(band + self.voxel)) / self.voxel;
        self.realize_voxels(
            [lo.x.floor() as i32, lo.y.floor() as i32, lo.z.floor() as i32],
            [hi.x.ceil() as i32, hi.y.ceil() as i32, hi.z.ceil() as i32],
        );
        let mut touched = Vec::new();
        for iz in lo.z.floor() as i32..=hi.z.ceil() as i32 {
            for iy in lo.y.floor() as i32..=hi.y.ceil() as i32 {
                for ix in lo.x.floor() as i32..=hi.x.ceil() as i32 {
                    let p = Vec3::new(ix as f32, iy as f32, iz as f32) * self.voxel;
                    let d = sdf(p);
                    if d >= band {
                        continue; // far air: the sentinel already says this
                    }
                    let cur = self.voxel_at([ix, iy, iz]);
                    if d.min(cur) >= cur - 1e-6 && d > -band {
                        continue; // no-op write near the surface: skip the materialize
                    }
                    self.set_voxel([ix, iy, iz], d.min(cur), Some(color(p)));
                    touched.push(chunk_of([ix, iy, iz]));
                }
            }
        }
        touched.sort_unstable();
        touched.dedup();
        // Generators routinely bend distances (noise-displaced spheres are not true
        // SDFs) — project back onto the 1-Lipschitz constraint like the Smooth brush
        // does, so raycasts and shadow marches stay honest.
        self.renormalize(&touched);
        self.compact(&touched);
    }

    /// Generate a fresh field over `[min, max]` from `src`, chunk by chunk on
    /// every core but one.
    ///
    /// The result is the field [`Self::fill_with_rgba`] would write into an empty
    /// field from the same distances and colours, voxel for voxel. What is
    /// different is what it costs: [`VoxelSource::classify`] answers for whole
    /// blocks of voxels first, and a block it can place wholly in open air or
    /// wholly in deep solid is never evaluated voxel by voxel. A planet's fill
    /// then costs the shell its surface and caves live in, not the cube around
    /// it, and that shell is shared out across threads.
    pub fn generate(voxel: f32, min: Vec3, max: Vec3, src: &impl VoxelSource) -> Self {
        Self::generate_with(voxel, min, max, src, false).0
    }

    /// [`Self::generate`], leaving for later every chunk `src` says nothing can
    /// see until it comes near ([`VoxelSource::defer`]): it is stored as the
    /// solid rock it is buried in, costs nothing, and is made the same as it
    /// would have been when [`Self::realize`] or [`Self::adopt_generated`] asks,
    /// or when anything writes into it. A planet's caves are most of its
    /// chunks, and a player sees the few they reach.
    pub fn generate_deferring<S: VoxelSource + Send + Sync + 'static>(
        voxel: f32,
        min: Vec3,
        max: Vec3,
        src: std::sync::Arc<S>,
    ) -> Self {
        Self::generate_sourced(voxel, min, max, src, true)
    }

    /// [`Self::generate`], deferring as [`Self::generate_deferring`] does when
    /// `defer` is set, and keeping `src` either way: what makes the deferred
    /// chunks, and what makes any chunk [`Self::evict`] put away again exactly
    /// as the fill made it.
    pub fn generate_sourced<S: VoxelSource + Send + Sync + 'static>(
        voxel: f32,
        min: Vec3,
        max: Vec3,
        src: std::sync::Arc<S>,
        defer: bool,
    ) -> Self {
        let (mut field, frame) = Self::generate_with(voxel, min, max, &*src, defer);
        let (ilo, ihi) = frame;
        let (voxel, band, base) = (field.voxel, field.band(), field.base_color);
        field.source = Some(Source(std::sync::Arc::new(Deferred { src, ilo, ihi, voxel, band, base, defer })));
        field
    }

    /// [`Self::generate_sourced`], making only the chunks `near` asks for: the
    /// rest of the surface stands in as coarse lattices sampled straight from
    /// `src`, pending, and is made exactly as the whole fill would have made it
    /// when something comes near ([`Self::realize`], or a worker through
    /// [`Self::pending_source`]). A planet kilometres across is ready in the
    /// time its stand-ins take, a fiftieth of a voxel each, and the ground
    /// around the player.
    pub fn generate_near<S: VoxelSource + Send + Sync + 'static>(
        voxel: f32,
        min: Vec3,
        max: Vec3,
        src: std::sync::Arc<S>,
        defer: bool,
        near: impl Fn([i32; 3]) -> bool + Sync,
    ) -> Self {
        let mut field = Self::new(voxel.clamp(0.25, 16.0));
        let (voxel, band, base) = (field.voxel, field.band(), field.base_color);
        let lo = (min - Vec3::splat(band + voxel)) / voxel;
        let hi = (max + Vec3::splat(band + voxel)) / voxel;
        let ilo = [lo.x.floor() as i32, lo.y.floor() as i32, lo.z.floor() as i32];
        let ihi = [hi.x.ceil() as i32, hi.y.ceil() as i32, hi.z.ceil() as i32];
        let maker = Deferred { src: src.clone(), ilo, ihi, voxel, band, base, defer };
        let (c0, c1) = (chunk_of(ilo), chunk_of(ihi));
        let mut coords = Vec::new();
        for cz in c0[2]..=c1[2] {
            for cy in c0[1]..=c1[1] {
                for cx in c0[0]..=c1[0] {
                    coords.push([cx, cy, cz]);
                }
            }
        }
        let at = |i: [i32; 3]| Vec3::new(i[0] as f32, i[1] as f32, i[2] as f32) * voxel;
        enum Made {
            Absent,
            Pending(Chunk),
            Ready(Option<Chunk>),
        }
        let made = par_map(coords.len(), |k| {
            let c = coords[k];
            let b = [c[0] * CHUNK, c[1] * CHUNK, c[2] * CHUNK];
            let e = [b[0] + CHUNK - 1, b[1] + CHUNK - 1, b[2] + CHUNK - 1];
            if maker.deferred(c) {
                return Made::Pending(Chunk::Uniform(-band));
            }
            let s = [0, 1, 2].map(|k| b[k].max(ilo[k]));
            let ec = [0, 1, 2].map(|k| e[k].min(ihi[k]));
            match src.classify(at(s), at(ec), band) {
                BlockClass::Air => return Made::Absent,
                BlockClass::Solid if s == b && ec == e => return Made::Ready(Some(Chunk::Uniform(-band))),
                _ => {}
            }
            if near(c) {
                return Made::Ready(maker.exact(c).0);
            }
            // The stand-in, from the generator's own samples.
            let n = LATTICE_N;
            let (mut dist, mut color) = (Vec::with_capacity(n * n * n), Vec::with_capacity(n * n * n));
            let mut scratch = S::Scratch::default();
            for z in 0..n {
                for y in 0..n {
                    for x in 0..n {
                        let st = LATTICE_STRIDE as i32;
                        let i = [b[0] + x as i32 * st, b[1] + y as i32 * st, b[2] + z as i32 * st];
                        let inside = (0..3).all(|k| i[k] >= ilo[k] && i[k] <= ihi[k]);
                        let (d, col) = match inside.then(|| src.sample(at(i), band, &mut scratch)).flatten() {
                            Some((d, col)) => (d.clamp(-band, band), col),
                            None => (band, base),
                        };
                        dist.push(d);
                        color.push(col);
                    }
                }
            }
            Made::Pending(Chunk::Lattice(std::sync::Arc::new(Lattice::new(dist, color))))
        });
        for (c, m) in coords.into_iter().zip(made) {
            match m {
                Made::Absent | Made::Ready(None) => {}
                Made::Ready(Some(ch)) => {
                    field.chunks.insert(c, ch);
                }
                Made::Pending(ch) => {
                    field.chunks.insert(c, ch);
                    field.pending.insert(c);
                }
            }
        }
        field.generated = true;
        field.source = Some(Source(std::sync::Arc::new(maker)));
        field
    }

    /// The source [`Self::generate_deferring`] would give a field filled over
    /// `[min, max]` from `src`, for one whose pending chunks came from a file.
    pub fn generate_source<S: VoxelSource + Send + Sync + 'static>(
        voxel: f32,
        min: Vec3,
        max: Vec3,
        src: std::sync::Arc<S>,
    ) -> std::sync::Arc<dyn PendingSource> {
        Self::generate_source_for(voxel, min, max, src, true)
    }

    /// The source [`Self::generate_sourced`] gives a field filled over
    /// `[min, max]` from `src`, deferring or not.
    pub fn generate_source_for<S: VoxelSource + Send + Sync + 'static>(
        voxel: f32,
        min: Vec3,
        max: Vec3,
        src: std::sync::Arc<S>,
        defer: bool,
    ) -> std::sync::Arc<dyn PendingSource> {
        let field = Self::new(voxel.clamp(0.25, 16.0));
        let (voxel, band, base) = (field.voxel, field.band(), field.base_color);
        let lo = (min - Vec3::splat(band + voxel)) / voxel;
        let hi = (max + Vec3::splat(band + voxel)) / voxel;
        let ilo = [lo.x.floor() as i32, lo.y.floor() as i32, lo.z.floor() as i32];
        let ihi = [hi.x.ceil() as i32, hi.y.ceil() as i32, hi.z.ceil() as i32];
        std::sync::Arc::new(Deferred { src, ilo, ihi, voxel, band, base, defer })
    }

    fn generate_with(
        voxel: f32,
        min: Vec3,
        max: Vec3,
        src: &impl VoxelSource,
        defer: bool,
    ) -> (Self, ([i32; 3], [i32; 3])) {
        let mut field = Self::new(voxel.clamp(0.25, 16.0));
        let (voxel, band) = (field.voxel, field.band());
        let lo = (min - Vec3::splat(band + voxel)) / voxel;
        let hi = (max + Vec3::splat(band + voxel)) / voxel;
        let ilo = [lo.x.floor() as i32, lo.y.floor() as i32, lo.z.floor() as i32];
        let ihi = [hi.x.ceil() as i32, hi.y.ceil() as i32, hi.z.ceil() as i32];
        let (c0, c1) = (chunk_of(ilo), chunk_of(ihi));
        let mut coords = Vec::new();
        for cz in c0[2]..=c1[2] {
            for cy in c0[1]..=c1[1] {
                for cx in c0[0]..=c1[0] {
                    coords.push([cx, cy, cz]);
                }
            }
        }
        let base = field.base_color;
        let at = |i: [i32; 3]| Vec3::new(i[0] as f32, i[1] as f32, i[2] as f32) * voxel;
        let chunks = par_map(coords.len(), |k| {
            let c = coords[k];
            let (s, e) = ([c[0] * CHUNK, c[1] * CHUNK, c[2] * CHUNK], [c[0] * CHUNK + CHUNK - 1, c[1] * CHUNK + CHUNK - 1, c[2] * CHUNK + CHUNK - 1]);
            if defer && src.defer(at(s), at(e), band) {
                return Err(());
            }
            Ok(gen_chunk(c, ilo, ihi, voxel, band, base, src))
        });
        let mut touched = Vec::new();
        for (c, chunk) in coords.into_iter().zip(chunks) {
            let Ok(chunk) = chunk else {
                field.chunks.insert(c, Chunk::Uniform(-band));
                field.pending.insert(c);
                continue;
            };
            if let Some(chunk) = chunk {
                if matches!(chunk, Chunk::Data(_)) {
                    touched.push(c);
                }
                field.chunks.insert(c, chunk);
            }
        }
        // As `fill_with_rgba`: project back onto 1-Lipschitz, then collapse what
        // came out uniform. The generator's own writes, not edits.
        field.quiet = true;
        field.renormalize(&touched);
        field.compact(&touched);
        field.quiet = false;
        field.generated = true;
        (field, (ilo, ihi))
    }

    /// How many chunks are not generated yet.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// The chunks not generated yet within the world box `lo..hi`.
    pub fn pending_in_box(&self, lo: Vec3, hi: Vec3) -> Vec<[i32; 3]> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        self.chunks_in_world_box(lo, hi).into_iter().filter(|c| self.pending.contains(c)).collect()
    }

    /// What makes this field's pending chunks, for running it off this thread:
    /// hand its answers to [`Self::adopt_generated`].
    pub fn pending_source(&self) -> Option<std::sync::Arc<dyn PendingSource>> {
        self.source.as_ref().map(|s| s.0.clone())
    }

    /// Give this field the source of its pending chunks: one loaded from a file
    /// knows which chunks are pending but not how to make them.
    pub fn set_pending_source(&mut self, source: std::sync::Arc<dyn PendingSource>) {
        self.source = Some(Source(source));
    }

    /// May chunk `c` be put away ([`Self::evict`])? A generated field's chunk
    /// that no edit has touched, holding data, with a source to make it again
    /// and a recipe its saves carry (a save of the whole field could not say
    /// what an evicted chunk was).
    pub fn evictable(&self, c: [i32; 3]) -> bool {
        self.generated
            && self.source.is_some()
            && self.spec.is_some()
            && !self.edited.contains(&c)
            && !self.pending.contains(&c)
            && matches!(self.chunks.get(&c), Some(Chunk::Data(_)))
    }

    /// Put these chunks away, where [`Self::evictable`]: each is replaced by a
    /// coarse stand-in of it, a few KB, and listed as pending, so that a write,
    /// [`Self::realize`] or a worker making it ([`Self::pending_source`],
    /// [`Self::adopt_generated`]) brings it back exactly as it was. A planet
    /// kilometres across keeps the ground near the player and stand-ins for
    /// the rest. Returns what changed, for [`Self::apply_evictions`] on the
    /// field's copies.
    pub fn evict(&mut self, coords: &[[i32; 3]]) -> Vec<Eviction> {
        let mut out = Vec::new();
        for &c in coords {
            if !self.evictable(c) {
                continue;
            }
            let b = [c[0] * CHUNK, c[1] * CHUNK, c[2] * CHUNK];
            let n = LATTICE_N;
            let (mut dist, mut color) = (Vec::with_capacity(n * n * n), Vec::with_capacity(n * n * n));
            for z in 0..n {
                for y in 0..n {
                    for x in 0..n {
                        let s = LATTICE_STRIDE as i32;
                        let i = [b[0] + x as i32 * s, b[1] + y as i32 * s, b[2] + z as i32 * s];
                        dist.push(self.voxel_at(i));
                        color.push(self.color_at(i));
                    }
                }
            }
            let new = Chunk::Lattice(std::sync::Arc::new(Lattice::new(dist, color)));
            if let Some(old) = self.chunks.insert(c, new.clone()) {
                self.pending.insert(c);
                out.push(Eviction { c, old, new });
            }
        }
        out
    }

    /// Put away in this copy the chunks another copy of the field put away,
    /// where this one still holds what that one did: the play session's
    /// snapshot and the physics sim's copy share the field's chunks, so a
    /// chunk is freed only once every copy lets go of it. A chunk this copy has
    /// written since is its own, and stays.
    pub fn apply_evictions(&mut self, evictions: &[Eviction]) {
        for ev in evictions {
            let same = match (self.chunks.get(&ev.c), &ev.old) {
                (Some(Chunk::Data(a)), Chunk::Data(b)) => std::sync::Arc::ptr_eq(a, b),
                _ => false,
            };
            if same {
                self.chunks.insert(ev.c, ev.new.clone());
                self.pending.insert(ev.c);
            }
        }
    }

    /// How many chunks are put away, standing in as lattices.
    pub fn evicted_count(&self) -> usize {
        self.chunks.values().filter(|c| matches!(c, Chunk::Lattice(_))).count()
    }

    /// Is chunk `c` put away ([`Self::evict`])?
    pub fn is_evicted(&self, c: [i32; 3]) -> bool {
        matches!(self.chunks.get(&c), Some(Chunk::Lattice(_)))
    }

    /// Generate these pending chunks now, on this thread's workers. Returns the
    /// chunks that changed, for a remesh and a collider copy.
    pub fn realize(&mut self, coords: &[[i32; 3]]) -> Vec<[i32; 3]> {
        let want: Vec<[i32; 3]> = coords.iter().copied().filter(|c| self.pending.contains(c)).collect();
        let Some(src) = self.pending_source() else { return Vec::new() };
        let made = par_map(want.len(), |k| src.generate(want[k]));
        self.adopt_generated(want.into_iter().zip(made).collect())
    }

    /// Take chunks a [`PendingSource`] made, wherever it ran. One that is no
    /// longer pending (an edit made it in the meantime) is dropped, since the
    /// edit is newer. Returns the chunks that changed.
    pub fn adopt_generated(&mut self, made: Vec<([i32; 3], Generated)>) -> Vec<[i32; 3]> {
        let mut touched = Vec::new();
        for (c, g) in made {
            if !self.pending.remove(&c) {
                continue;
            }
            match g.0 {
                Some(chunk) => {
                    self.chunks.insert(c, chunk);
                }
                None => {
                    self.chunks.remove(&c);
                }
            }
            self.note_changed([c[0] * CHUNK, c[1] * CHUNK, c[2] * CHUNK]);
            self.note_changed([c[0] * CHUNK + CHUNK - 1, c[1] * CHUNK + CHUNK - 1, c[2] * CHUNK + CHUNK - 1]);
            touched.push(c);
        }
        // A neighbour's mesh reads a layer into a made chunk, so it changes too.
        let mut region = touched.clone();
        for c in &touched {
            for d in 0..27 {
                region.push([c[0] + d % 3 - 1, c[1] + d / 3 % 3 - 1, c[2] + d / 9 - 1]);
            }
        }
        region.sort_unstable();
        region.dedup();
        region.retain(|c| self.chunks.contains_key(c));
        region
    }

    /// Generate, on this thread, the pending chunks a write over voxels
    /// `lo..=hi` reads or writes, with the voxels its projection can reach.
    fn realize_voxels(&mut self, lo: [i32; 3], hi: [i32; 3]) {
        if self.pending.is_empty() {
            return;
        }
        let m = 8;
        let (c0, c1) = (chunk_of([lo[0] - m, lo[1] - m, lo[2] - m]), chunk_of([hi[0] + m, hi[1] + m, hi[2] + m]));
        let mut want = Vec::new();
        for z in c0[2]..=c1[2] {
            for y in c0[1]..=c1[1] {
                for x in c0[0]..=c1[0] {
                    if self.pending.contains(&[x, y, z]) {
                        want.push([x, y, z]);
                    }
                }
            }
        }
        if !want.is_empty() {
            self.realize(&want);
        }
    }

    /// [`Self::realize`] from inside a write, with no projection: the write in
    /// progress runs its own.
    fn realize_now(&mut self, coords: &[[i32; 3]]) {
        let Some(src) = self.pending_source() else {
            for c in coords {
                self.pending.remove(c);
            }
            return;
        };
        for &c in coords {
            if self.pending.remove(&c) {
                match src.generate(c).0 {
                    Some(chunk) => {
                        self.chunks.insert(c, chunk);
                    }
                    None => {
                        self.chunks.remove(&c);
                    }
                }
            }
        }
    }

    /// Collapse fully-saturated chunks to sentinels. Called automatically by the brushes;
    /// exposed for bulk writers that use [`Self::write_voxel`] directly.
    pub fn compact_all(&mut self) {
        let coords: Vec<[i32; 3]> = self.chunks.keys().copied().collect();
        self.compact(&coords);
    }

    /// Bulk-copy a voxel box into flat scratch buffers, `dim³`, x-fastest, from `lo`.
    ///
    /// This exists for one reason: **speed**. Sampling through [`Self::voxel_at`] costs a
    /// HashMap lookup per voxel, and the mesher needs ~48 per vertex for the gradient
    /// alone — measured at 11 ms/chunk, 11× over budget. Here the ≤27 overlapping chunks
    /// are visited once each and their rows copied, so the mesher's inner loops become
    /// array indexing. Same values, same global-voxel addressing, no lookups.
    pub fn gather(&self, lo: [i32; 3], dim: usize, dist: &mut Vec<f32>, color: &mut Vec<[u8; 4]>) {
        let band = self.band();
        dist.clear();
        dist.resize(dim * dim * dim, band);
        color.clear();
        color.resize(dim * dim * dim, self.base_color);
        let hi = [lo[0] + dim as i32 - 1, lo[1] + dim as i32 - 1, lo[2] + dim as i32 - 1];
        let (c0, c1) = (chunk_of(lo), chunk_of(hi));
        for cz in c0[2]..=c1[2] {
            for cy in c0[1]..=c1[1] {
                for cx in c0[0]..=c1[0] {
                    let cc = [cx, cy, cz];
                    let (bx, by, bz) = (cx * CHUNK, cy * CHUNK, cz * CHUNK);
                    let sx = lo[0].max(bx);
                    let sy = lo[1].max(by);
                    let sz = lo[2].max(bz);
                    let ex = hi[0].min(bx + CHUNK - 1);
                    let ey = hi[1].min(by + CHUNK - 1);
                    let ez = hi[2].min(bz + CHUNK - 1);
                    if sx > ex || sy > ey || sz > ez {
                        continue;
                    }
                    match self.chunks.get(&cc) {
                        // Absent regions already hold the right constant from `resize`:
                        // the sparse store paying off in the hot loop, not just in RAM.
                        None => {}
                        Some(Chunk::Uniform(v)) => {
                            for z in sz..=ez {
                                for y in sy..=ey {
                                    let di = ((z - lo[2]) as usize * dim + (y - lo[1]) as usize) * dim;
                                    let a = di + (sx - lo[0]) as usize;
                                    let b = di + (ex - lo[0]) as usize;
                                    dist[a..=b].fill(*v);
                                }
                            }
                        }
                        Some(Chunk::Lattice(l)) => {
                            for z in sz..=ez {
                                for y in sy..=ey {
                                    for x in sx..=ex {
                                        let di = ((z - lo[2]) as usize * dim + (y - lo[1]) as usize) * dim + (x - lo[0]) as usize;
                                        let li = local_of([x, y, z]);
                                        dist[di] = l.dist(li);
                                        color[di] = l.color(li);
                                    }
                                }
                            }
                        }
                        Some(Chunk::Data(d)) => {
                            let at = ((sz - lo[2]) as usize * dim + (sy - lo[1]) as usize) * dim + (sx - lo[0]) as usize;
                            d.copy_box(
                                [(sx - bx) as usize, (sy - by) as usize, (sz - bz) as usize],
                                [(ex - bx) as usize, (ey - by) as usize, (ez - bz) as usize],
                                dim,
                                at,
                                dist,
                                color,
                                band,
                                self.base_color,
                            );
                        }
                    }
                }
            }
        }
    }

    // ---- raycast ------------------------------------------------------------

    /// First surface hit along a ray, or `None`. Used by the sculpt brush (replacing the
    /// dense grid's raycast) and available to physics.
    ///
    /// Sphere-traces the trilinear field at half-steps (the field is not perfectly
    /// 1-Lipschitz — see `renormalize`). The band clamp caps each step at `band`, which
    /// costs a few extra steps through open air and buys the sparse store — a trade the
    /// dense field could not make.
    pub fn raycast(&self, ro: Vec3, rd: Vec3, max_t: f32) -> Option<Vec3> {
        let rd = rd.normalize_or_zero();
        if rd == Vec3::ZERO {
            return None;
        }
        let eps = self.voxel * 0.05;
        let mut t = 0.0f32;
        // If we start inside solid there is no "first surface" ahead to speak of.
        if self.d(ro) < 0.0 {
            return None;
        }
        for _ in 0..2048 {
            if t > max_t {
                return None;
            }
            let p = ro + rd * t;
            let d = self.d(p);
            if d < eps {
                // Bisect back onto the isosurface: the last step overshot into solid, so
                // the crossing is bracketed and a few halvings land within a fraction of
                // a voxel.
                let (mut a, mut b) = (t - self.voxel.max(d.abs()), t);
                for _ in 0..24 {
                    let m = 0.5 * (a + b);
                    if self.d(ro + rd * m) < 0.0 {
                        b = m;
                    } else {
                        a = m;
                    }
                }
                return Some(ro + rd * b);
            }
            // Step a fraction of the reported distance. Sphere tracing assumes
            // |∇d| ≤ 1; `fill_slab` and the CSG Raise/Lower hold that exactly, but the
            // Smooth/Flatten blends can still leave ~2-5 locally (see
            // `brush_writes_keep_the_field_1_lipschitz`), and a full-length step there
            // would sail through the surface. Halving costs a few iterations and makes
            // the march robust to the field it actually has rather than the one it
            // wishes it had.
            t += (d * 0.5).max(self.voxel * 0.25);
        }
        None
    }

    /// How far along a ray it first comes within `margin` of the surface, or
    /// `None` when it never does within `max_t`. `Some(0.0)` when it starts
    /// that close.
    ///
    /// What a collider asks before an exact test against geometry drawn from
    /// this field: a mesh built from it sits within about half a voxel of the
    /// surface, so a ray that never comes within a voxel of the surface cannot
    /// touch the mesh either, and one that does is worth testing from there.
    /// The plain [`raycast`](Self::raycast) answers the surface itself, and a ray
    /// grazing a bump passed over the field's surface while clipping the mesh.
    pub fn raycast_within(&self, ro: Vec3, rd: Vec3, max_t: f32, margin: f32) -> Option<f32> {
        let rd = rd.normalize_or_zero();
        if rd == Vec3::ZERO {
            return None;
        }
        let mut t = 0.0f32;
        for _ in 0..2048 {
            if t > max_t {
                return None;
            }
            let d = self.d(ro + rd * t) - margin;
            if d < 0.0 {
                return Some(t);
            }
            // Half steps, for the same reason `raycast` takes them.
            t += (d * 0.5).max(self.voxel * 0.25);
        }
        None
    }

    // ---- migration ----------------------------------------------------------

    /// Import a dense [`crate::mesh2sdf::BakedSdf`] (a legacy `.tfield`) into the sparse
    /// store, resampling onto this field's voxel lattice.
    ///
    /// Existing projects must just open. The dense grid's values are trilinearly
    /// resampled and band-clamped; uniform regions collapse. a 192 MB field imports to
    /// single-digit MB precisely because everything outside the band was never worth
    /// storing.
    pub fn from_dense(baked: &crate::mesh2sdf::BakedSdf, voxel: f32) -> Self {
        let mut f = Self::new(voxel);
        let half = Vec3::from(baked.half_extent);
        let center = Vec3::from(baked.center);
        let lo = center - half;
        let ilo = [
            (lo.x / voxel).floor() as i32,
            (lo.y / voxel).floor() as i32,
            (lo.z / voxel).floor() as i32,
        ];
        let ihi = [
            ((lo.x + 2.0 * half.x) / voxel).ceil() as i32,
            ((lo.y + 2.0 * half.y) / voxel).ceil() as i32,
            ((lo.z + 2.0 * half.z) / voxel).ceil() as i32,
        ];
        // Full import skips air (a fresh chunk is already `Uniform(+band)` air) so the
        // field stays sparse — only the surface band is materialized.
        f.resample_dense(baked, ilo, ihi, false);
        f
    }

    /// Re-sample the dense field over a world-space AABB, rewriting only the voxels inside
    /// it — the regional counterpart of [`from_dense`], for live editing. A brush dab moves
    /// a small box of the dense authority (which stays the source of truth for physics/atlas
    /// /save); this re-derives only the chunks overlapping that box so the render mesh keeps
    /// up without paying a full-field resample per dab. Returns the touched chunk coords
    /// (the remesh set). `min`/`max` are world coordinates in the dense field's frame.
    pub fn refresh_from_dense_region(
        &mut self,
        baked: &crate::mesh2sdf::BakedSdf,
        min: Vec3,
        max: Vec3,
    ) -> Vec<[i32; 3]> {
        // Pad by a voxel so the box's own rim + the mesher's apron are re-derived too.
        let v = self.voxel;
        let ilo = [
            (min.x / v).floor() as i32 - 1,
            (min.y / v).floor() as i32 - 1,
            (min.z / v).floor() as i32 - 1,
        ];
        let ihi = [
            (max.x / v).ceil() as i32 + 1,
            (max.y / v).ceil() as i32 + 1,
            (max.z / v).ceil() as i32 + 1,
        ];
        // Regional refresh writes air (a dig turned solid into air — the voxels that were
        // rock must be erased, or the old surface lingers) but only over already-touched
        // chunks; empty ones compact away.
        self.resample_dense(baked, ilo, ihi, true)
    }

    /// Shared core of [`from_dense`] / [`refresh_from_dense_region`]: trilinearly resample
    /// the dense field over the inclusive voxel-index box `[ilo, ihi]`, write each voxel,
    /// and compact. `write_air` distinguishes the two callers (see their comments). Returns
    /// the touched chunk coords.
    fn resample_dense(
        &mut self,
        baked: &crate::mesh2sdf::BakedSdf,
        ilo: [i32; 3],
        ihi: [i32; 3],
        write_air: bool,
    ) -> Vec<[i32; 3]> {
        let voxel = self.voxel;
        let band = self.band();
        let [w, h, d] = baked.dims;
        let lo = Vec3::from(baked.center) - Vec3::from(baked.half_extent);
        let vs = Vec3::new(
            2.0 * baked.half_extent[0] / (w.max(2) - 1) as f32,
            2.0 * baked.half_extent[1] / (h.max(2) - 1) as f32,
            2.0 * baked.half_extent[2] / (d.max(2) - 1) as f32,
        );
        let sample = |p: Vec3| -> (f32, [u8; 4]) {
            let g = (p - lo) / vs;
            let b = [
                (g.x.floor() as i32).clamp(0, w as i32 - 1),
                (g.y.floor() as i32).clamp(0, h as i32 - 1),
                (g.z.floor() as i32).clamp(0, d as i32 - 1),
            ];
            let fr = Vec3::new(g.x - b[0] as f32, g.y - b[1] as f32, g.z - b[2] as f32)
                .clamp(Vec3::ZERO, Vec3::ONE);
            let at = |dx: i32, dy: i32, dz: i32| {
                let x = (b[0] + dx).clamp(0, w as i32 - 1) as u32;
                let y = (b[1] + dy).clamp(0, h as i32 - 1) as u32;
                let z = (b[2] + dz).clamp(0, d as i32 - 1) as u32;
                ((z * h + y) * w + x) as usize
            };
            let l = |a: f32, b: f32, t: f32| a + (b - a) * t;
            let c = |dx, dy, dz| baked.distance[at(dx, dy, dz)];
            let x00 = l(c(0, 0, 0), c(1, 0, 0), fr.x);
            let x10 = l(c(0, 1, 0), c(1, 1, 0), fr.x);
            let x01 = l(c(0, 0, 1), c(1, 0, 1), fr.x);
            let x11 = l(c(0, 1, 1), c(1, 1, 1), fr.x);
            (l(l(x00, x10, fr.y), l(x01, x11, fr.y), fr.z), baked.color[at(0, 0, 0)])
        };
        let mut touched = Vec::new();
        for iz in ilo[2]..=ihi[2] {
            for iy in ilo[1]..=ihi[1] {
                for ix in ilo[0]..=ihi[0] {
                    let p = Vec3::new(ix as f32, iy as f32, iz as f32) * voxel;
                    let (dv, col) = sample(p);
                    // Skip far-from-surface AIR only — never far-from-surface solid. A fresh
                    // chunk starts as `Uniform(+band)` air, so skipping air writes what's
                    // already there; skipping deep solid (`dv <= -band`) left rock reading as
                    // air and the import became a hollow shell (a spurious inner surface a
                    // band below the real one — meshed vertices sat -0.935 units inside the
                    // dense field). Deep-solid voxels cost nothing: `compact` collapses a
                    // saturated chunk to a `Uniform(-band)` sentinel. (Clamping against the
                    // source box SDF was tried and made it worse: 4.8% -> 18.5%.)
                    if dv >= band {
                        // Regional refresh: overwrite air so a DIG (solid→air) actually
                        // erases the old surface. Full import: skip — the chunk is already
                        // air and writing would materialize it and defeat sparsity.
                        if write_air {
                            self.set_voxel([ix, iy, iz], band, Some(col));
                            touched.push(chunk_of([ix, iy, iz]));
                        }
                        continue;
                    }
                    self.set_voxel([ix, iy, iz], dv, Some(col));
                    touched.push(chunk_of([ix, iy, iz]));
                }
            }
        }
        touched.sort_unstable();
        touched.dedup();
        self.compact(&touched);
        touched
    }

    // ---- whole-field ops (the Fill tools) -------------------------------------

    /// Recolor every stored voxel (and the base colour) — "fill terrain with this
    /// color". Leaves shape and painted texture slots (alpha) alone.
    pub fn fill_color(&mut self, color: [f32; 3]) {
        let rgb = [
            (color[0] * 255.0).round().clamp(0.0, 255.0) as u8,
            (color[1] * 255.0).round().clamp(0.0, 255.0) as u8,
            (color[2] * 255.0).round().clamp(0.0, 255.0) as u8,
        ];
        self.base_color = [rgb[0], rgb[1], rgb[2], self.base_color[3]];
        self.edited.extend(self.chunks.keys().copied());
        for c in self.chunks.values_mut() {
            if let Chunk::Data(d) = c {
                std::sync::Arc::make_mut(d).map_colors(|v| [rgb[0], rgb[1], rgb[2], v[3]]);
            }
        }
    }

    /// Fill the whole terrain with a texture palette `slot` (1-based; 0 = untextured).
    /// The slot rides the colour alpha channel — same convention as the dense field
    /// and the splat shader. Leaves shape + RGB tint.
    pub fn fill_texture(&mut self, slot: u8) {
        self.base_color[3] = slot;
        self.edited.extend(self.chunks.keys().copied());
        for c in self.chunks.values_mut() {
            if let Chunk::Data(d) = c {
                std::sync::Arc::make_mut(d).map_colors(|v| [v[0], v[1], v[2], slot]);
            }
        }
    }

    /// Paint a texture palette `slot` (alpha channel) over near-surface voxels inside
    /// the brush ball. Mirrors the dense [`crate::Terrain::paint_texture`].
    pub fn paint_texture(&mut self, center: Vec3, radius: f32, slot: u8) -> Vec<[i32; 3]> {
        let (lo, hi) = self.voxel_range(center, radius);
        self.realize_voxels(lo, hi);
        let band = self.band();
        let mut touched = Vec::new();
        for iz in lo[2]..=hi[2] {
            for iy in lo[1]..=hi[1] {
                for ix in lo[0]..=hi[0] {
                    let p = Vec3::new(ix as f32, iy as f32, iz as f32) * self.voxel;
                    if (p - center).length() > radius {
                        continue;
                    }
                    // Only near the surface — the band is all anyone can ever see.
                    let d = self.voxel_at([ix, iy, iz]);
                    if d.abs() > band {
                        continue;
                    }
                    let mut c = self.color_at([ix, iy, iz]);
                    if c[3] == slot {
                        continue;
                    }
                    c[3] = slot;
                    self.set_voxel([ix, iy, iz], d, Some(c));
                    touched.push(chunk_of([ix, iy, iz]));
                }
            }
        }
        touched.sort_unstable();
        touched.dedup();
        touched
    }

    /// Lay flat ground across the field's current bounds ("fill bounds"): a solid slab
    /// from `floor_y` up to `top_y`, inset from the X/Z rim, unioned with what's there.
    pub fn fill_bounds(&mut self, top_y: f32, floor_y: f32, inset: f32, color: [f32; 3]) {
        let Some((lo, hi)) = self.bounds() else { return };
        let min = Vec3::new(lo.x + inset, floor_y.min(top_y), lo.z + inset);
        let max = Vec3::new(hi.x - inset, top_y, hi.z - inset);
        if min.x >= max.x || min.z >= max.z {
            return;
        }
        self.fill_slab(min, max, top_y.max(floor_y), color);
    }

    /// World-space AABB of everything stored (data chunks and solid interior
    /// sentinels), or `None` for an empty field. This is the box the shadow proxy,
    /// the collider wireframe, and camera framing use — an unbounded field still has
    /// bounded *content*.
    pub fn bounds(&self) -> Option<(Vec3, Vec3)> {
        let mut lo = [i32::MAX; 3];
        let mut hi = [i32::MIN; 3];
        for (c, ch) in &self.chunks {
            let solid = match ch {
                Chunk::Data(_) | Chunk::Lattice(_) => true,
                Chunk::Uniform(v) => *v < 0.0,
            };
            if !solid {
                continue;
            }
            for k in 0..3 {
                lo[k] = lo[k].min(c[k]);
                hi[k] = hi[k].max(c[k]);
            }
        }
        if lo[0] > hi[0] {
            return None;
        }
        let s = CHUNK as f32 * self.voxel;
        Some((
            Vec3::new(lo[0] as f32, lo[1] as f32, lo[2] as f32) * s,
            Vec3::new((hi[0] + 1) as f32, (hi[1] + 1) as f32, (hi[2] + 1) as f32) * s,
        ))
    }

    // ---- undo: per-stroke chunk snapshots -------------------------------------

    /// Every chunk coordinate a brush of `radius` at `center` could touch (its voxel
    /// range, chunk-rounded, plus the one-chunk ring `renormalize` may write into) —
    /// the pre-dab snapshot set. Includes absent coords: undo must also remember that
    /// a chunk did not exist.
    pub fn chunks_in_box(&self, center: Vec3, radius: f32) -> Vec<[i32; 3]> {
        let (lo, hi) = self.voxel_range(center, radius);
        let (mut c0, mut c1) = (chunk_of(lo), chunk_of(hi));
        for k in 0..3 {
            c0[k] -= 1; // renormalize's neighbour ring (Smooth/Flatten)
            c1[k] += 1;
        }
        let mut out = Vec::new();
        for cz in c0[2]..=c1[2] {
            for cy in c0[1]..=c1[1] {
                for cx in c0[0]..=c1[0] {
                    out.push([cx, cy, cz]);
                }
            }
        }
        out
    }

    /// Clone the current contents of `coords` into an undo record. A typical stroke
    /// touches 1–8 chunks ≈ 0.3–2.5 MB — versus the 192 MB the dense field's
    /// whole-field snapshot cost per stroke.
    pub fn snapshot_chunks(&self, coords: &[[i32; 3]]) -> ChunkUndo {
        ChunkUndo {
            entries: coords.iter().map(|c| (*c, self.chunks.get(c).cloned())).collect(),
        }
    }

    /// Restore the chunks recorded in `undo`, returning the inverse record (what was
    /// there instead) — so undo/redo is a value swap, exactly like the scene history.
    pub fn apply_undo(&mut self, undo: &ChunkUndo) -> ChunkUndo {
        let inverse = ChunkUndo {
            entries: undo
                .entries
                .iter()
                .map(|(c, _)| (*c, self.chunks.get(c).cloned()))
                .collect(),
        };
        for (c, ch) in &undo.entries {
            // Whole chunks change hands, so the whole chunk changed.
            self.note_changed([c[0] * CHUNK, c[1] * CHUNK, c[2] * CHUNK]);
            self.note_changed([c[0] * CHUNK + CHUNK - 1, c[1] * CHUNK + CHUNK - 1, c[2] * CHUNK + CHUNK - 1]);
            self.edited.insert(*c);
            match ch {
                Some(ch) => {
                    if matches!(ch, Chunk::Lattice(_)) {
                        self.pending.insert(*c);
                    } else {
                        self.pending.remove(c);
                    }
                    self.chunks.insert(*c, ch.clone());
                }
                None => {
                    self.chunks.remove(c);
                }
            }
        }
        inverse
    }

    // ---- persistence -----------------------------------------------------------

    /// Serialize to a compact `.cfield` blob. Distances quantize to i8 in band units
    /// (≤ voxel/32 error — far under the mesher's 0.3-voxel acceptance) and both
    /// channels RLE-encode, so the band's saturated plateaus cost almost nothing.
    /// Air-uniform chunks are implicit (absent == air) and never written.
    ///
    /// This walks every stored voxel — a dug-up planet takes hundreds of ms.
    /// Anything that runs per-frame must use [`Self::begin_save`] /
    /// [`Self::save_step`] instead and spread the walk across frames.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut s = self.begin_save();
        self.save_step(&mut s, usize::MAX);
        s.finish()
    }

    /// Start an incremental save: captures the chunk list + header. Encode with
    /// [`Self::save_step`] a budget at a time, then [`FieldSaver::finish`].
    ///
    /// The field may mutate between steps: each chunk encodes atomically from
    /// its state at its step (a chunk dug to air since `begin_save` is simply
    /// skipped), so the finished blob is always a valid field — at worst a
    /// torn snapshot mixing edit generations. Callers detect that (edit stamp)
    /// and keep such a field dirty for the next checkpoint.
    pub fn begin_save(&self) -> FieldSaver {
        // A generated field whose recipe is known saves what was done to it and
        // the recipe: the rest is made again on load. A dug 800 m planet wrote
        // its whole 400 MB field on every save.
        let delta = self.generated && self.spec.is_some();
        let mut buf = Vec::with_capacity(64 * 1024);
        buf.extend_from_slice(b"FCF1");
        buf.extend_from_slice(&(if delta { 2u32 } else { 1u32 }).to_le_bytes());
        buf.extend_from_slice(&self.voxel.to_le_bytes());
        buf.extend_from_slice(&self.base_color);
        let band = self.band();
        // Stored chunks: data + non-air uniforms (solid interior). Air is implicit.
        let mut coords: Vec<[i32; 3]> = if delta {
            self.edited.iter().copied().collect()
        } else {
            self.chunks
                .iter()
                .filter(|(_, ch)| match ch {
                    Chunk::Data(_) => true,
                    Chunk::Uniform(v) => *v < band - 1e-6,
                    // Put away, so made again from the source: listed as pending.
                    Chunk::Lattice(_) => false,
                })
                .map(|(c, _)| *c)
                .collect()
        };
        coords.sort_unstable(); // deterministic output (byte-identical saves)
        let count_pos = buf.len();
        buf.extend_from_slice(&0u32.to_le_bytes()); // patched by finish()
        // A delta's pending chunks are the generator's, made again with it.
        let mut pending: Vec<[i32; 3]> = if delta { Vec::new() } else { self.pending.iter().copied().collect() };
        pending.sort_unstable();
        let spec = if delta { self.spec.clone() } else { None };
        FieldSaver { delta, spec, coords, at: 0, written: 0, count_pos, buf, pending }
    }

    /// Encode up to `max_chunks` more chunks into `s`. Returns `true` once the
    /// blob is complete (call [`FieldSaver::finish`] to take the bytes).
    pub fn save_step(&self, s: &mut FieldSaver, max_chunks: usize) -> bool {
        let band = self.band();
        let end = s.at.saturating_add(max_chunks).min(s.coords.len());
        while s.at < end {
            let c = s.coords[s.at];
            s.at += 1;
            // Edited away since begin_save (dug to implicit air): drop out —
            // unless only edits are written, where air has to say so.
            let air = Chunk::Uniform(band);
            let ch = match self.chunks.get(&c) {
                Some(Chunk::Uniform(v)) if *v >= band - 1e-6 && !s.delta => continue,
                // Put away since `begin_save`: made again from the source, so
                // nothing of it is the save's to keep.
                Some(Chunk::Lattice(_)) => continue,
                Some(ch) => ch,
                None if s.delta => &air,
                None => continue,
            };
            for k in c {
                s.buf.extend_from_slice(&k.to_le_bytes());
            }
            match ch {
                Chunk::Lattice(_) => {}
                Chunk::Uniform(v) => {
                    s.buf.push(0);
                    s.buf.extend_from_slice(&v.to_le_bytes());
                }
                Chunk::Data(d) => {
                    s.buf.push(1);
                    // Distance: i8 in band units, RLE (u16 run, i8 value).
                    let q = |v: f32| ((v / band) * 127.0).round().clamp(-127.0, 127.0) as i8;
                    let mut runs: Vec<(u16, i8)> = Vec::new();
                    for v in &(0..CHUNK_VOXELS).map(|li| d.dist(li)).collect::<Vec<_>>() {
                        let qv = q(*v);
                        match runs.last_mut() {
                            Some((n, lv)) if *lv == qv && *n < u16::MAX => *n += 1,
                            _ => runs.push((1, qv)),
                        }
                    }
                    s.buf.extend_from_slice(&(runs.len() as u32).to_le_bytes());
                    for (n, v) in runs {
                        s.buf.extend_from_slice(&n.to_le_bytes());
                        s.buf.push(v as u8);
                    }
                    // Colour: RLE (u16 run, RGBA8).
                    let mut cruns: Vec<(u16, [u8; 4])> = Vec::new();
                    for v in &(0..CHUNK_VOXELS).map(|li| d.color(li)).collect::<Vec<_>>() {
                        match cruns.last_mut() {
                            Some((n, lv)) if lv == v && *n < u16::MAX => *n += 1,
                            _ => cruns.push((1, *v)),
                        }
                    }
                    s.buf.extend_from_slice(&(cruns.len() as u32).to_le_bytes());
                    for (n, v) in cruns {
                        s.buf.extend_from_slice(&n.to_le_bytes());
                        s.buf.extend_from_slice(&v);
                    }
                }
            }
            s.written += 1;
        }
        s.at == s.coords.len()
    }

    /// Parse a `.cfield` blob written by [`Self::to_bytes`]. `None` on any malformed
    /// input — the caller falls back exactly as it does for a garbled `.tfield`.
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        let mut at = 0usize;
        let take = |at: &mut usize, n: usize| -> Option<&[u8]> {
            let s = data.get(*at..*at + n)?;
            *at += n;
            Some(s)
        };
        if take(&mut at, 4)? != b"FCF1" {
            return None;
        }
        let u32_at = |s: &[u8]| u32::from_le_bytes(s.try_into().ok().unwrap_or([0; 4]));
        let version = u32_at(take(&mut at, 4)?);
        if version != 1 && version != 2 {
            return None;
        }
        let voxel = f32::from_le_bytes(take(&mut at, 4)?.try_into().ok()?);
        if !(voxel.is_finite() && voxel > 1e-3) {
            return None;
        }
        let mut f = Self::new(voxel);
        f.delta = version == 2;
        f.base_color = take(&mut at, 4)?.try_into().ok()?;
        let band = f.band();
        let count = u32_at(take(&mut at, 4)?) as usize;
        if count > 4_000_000 {
            return None; // corrupt count guard: nobody has 4M chunks
        }
        for _ in 0..count {
            let mut c = [0i32; 3];
            for k in &mut c {
                *k = i32::from_le_bytes(take(&mut at, 4)?.try_into().ok()?);
            }
            match take(&mut at, 1)?[0] {
                0 => {
                    let v = f32::from_le_bytes(take(&mut at, 4)?.try_into().ok()?);
                    if !v.is_finite() {
                        return None;
                    }
                    f.chunks.insert(c, Chunk::Uniform(v.clamp(-band, band)));
                }
                1 => {
                    let mut dist = Vec::with_capacity(CHUNK_VOXELS);
                    let nruns = u32_at(take(&mut at, 4)?) as usize;
                    for _ in 0..nruns {
                        let n = u16::from_le_bytes(take(&mut at, 2)?.try_into().ok()?) as usize;
                        let v = take(&mut at, 1)?[0] as i8;
                        let d = v as f32 / 127.0 * band;
                        if dist.len() + n > CHUNK_VOXELS {
                            return None;
                        }
                        dist.resize(dist.len() + n, d);
                    }
                    if dist.len() != CHUNK_VOXELS {
                        return None;
                    }
                    let mut color = Vec::with_capacity(CHUNK_VOXELS);
                    let ncruns = u32_at(take(&mut at, 4)?) as usize;
                    for _ in 0..ncruns {
                        let n = u16::from_le_bytes(take(&mut at, 2)?.try_into().ok()?) as usize;
                        let v: [u8; 4] = take(&mut at, 4)?.try_into().ok()?;
                        if color.len() + n > CHUNK_VOXELS {
                            return None;
                        }
                        color.resize(color.len() + n, v);
                    }
                    if color.len() != CHUNK_VOXELS {
                        return None;
                    }
                    f.chunks.insert(c, Chunk::Data(std::sync::Arc::new(ChunkData::from_flat(&dist, &color))));
                }
                _ => return None,
            }
            if f.delta {
                f.edited.insert(c);
            }
        }
        if data.get(at..at + 4) == Some(b"SPEC".as_slice()) {
            at += 4;
            let n = u32_at(take(&mut at, 4)?) as usize;
            f.spec = Some(String::from_utf8(take(&mut at, n)?.to_vec()).ok()?);
        }
        if f.delta && f.spec.is_none() {
            return None; // edits with nothing to lay them over
        }
        if data.get(at..at + 4) == Some(b"PEND".as_slice()) {
            at += 4;
            let n = u32_at(take(&mut at, 4)?) as usize;
            if n > 4_000_000 {
                return None;
            }
            for _ in 0..n {
                let mut c = [0i32; 3];
                for k in &mut c {
                    *k = i32::from_le_bytes(take(&mut at, 4)?.try_into().ok()?);
                }
                f.pending.insert(c);
            }
        }
        Some(f)
    }

    // ---- shadow proxy (dense downsample for the SDF atlas) ----------------------

    /// Downsample the sparse field into a dense [`crate::mesh2sdf::BakedSdf`] for the
    /// GPU shadow/AO atlas — the interim stand-in until the P5 shadow clipmap. Longest
    /// axis capped at `max_dim` cells, so the proxy's cost is bounded no matter how
    /// large the map grows (soft sun shadows are visually forgiving of a coarse field;
    /// primary visibility — the unforgiving part — is the chunk meshes, not this).
    ///
    /// Stored distances saturate at ±band, which would cap every shadow-march step at
    /// a few units and starve the march's iteration budget across a big map — so after
    /// sampling, a two-pass chamfer sweep re-expands the AIR side into true-ish
    /// distance. (The solid side stays clamped; nothing marches through rock.)
    pub fn to_dense(&self, max_dim: u32) -> Option<crate::mesh2sdf::BakedSdf> {
        let (lo, hi) = self.bounds()?;
        let pad = self.band() * 1.5;
        let (lo, hi) = (lo - Vec3::splat(pad), hi + Vec3::splat(pad));
        let ext = hi - lo;
        let vp = (ext.max_element() / max_dim.max(2) as f32).max(self.voxel);
        // Voxel centers at (i+0.5)/n across the box — the convention `Terrain::sample`,
        // the atlas upload, and the GPU field shader all share.
        let dims = [
            ((ext.x / vp).ceil() as u32).max(2),
            ((ext.y / vp).ceil() as u32).max(2),
            ((ext.z / vp).ceil() as u32).max(2),
        ];
        let n = dims[0] as usize * dims[1] as usize * dims[2] as usize;
        let mut baked = crate::mesh2sdf::BakedSdf {
            dims,
            center: ((lo + hi) * 0.5).to_array(),
            half_extent: (ext * 0.5).to_array(),
            distance: vec![0.0; n],
            color: vec![self.base_color; n],
        };
        self.sample_into_dense(&mut baked, [0, 0, 0], [dims[0] - 1, dims[1] - 1, dims[2] - 1]);
        chamfer_expand_air(&mut baked, self.band());
        Some(baked)
    }

    /// Re-sample a world-space box of this field into an existing proxy, returning the
    /// proxy voxel index box it rewrote (inclusive min, exclusive max — the exact
    /// arguments the atlas's `set_volume_region` wants). The dab fast path.
    pub fn refresh_dense_region(
        &self,
        baked: &mut crate::mesh2sdf::BakedSdf,
        wmin: Vec3,
        wmax: Vec3,
    ) -> ([u32; 3], [u32; 3]) {
        self.sample_dense_region(baked.dims, baked.center, baked.half_extent, wmin, wmax).write_into(baked)
    }

    /// [`Self::refresh_dense_region`]'s samples, for a proxy with this grid,
    /// without the proxy: taken where the field is (a worker's copy of it) and
    /// written into the proxy later by [`DenseRegion::write_into`].
    pub fn sample_dense_region(&self, dims: [u32; 3], center: [f32; 3], half_extent: [f32; 3], wmin: Vec3, wmax: Vec3) -> DenseRegion {
        let [w, h, d] = dims;
        let lo = Vec3::from(center) - Vec3::from(half_extent);
        let vs = Vec3::new(
            2.0 * half_extent[0] / w.max(1) as f32,
            2.0 * half_extent[1] / h.max(1) as f32,
            2.0 * half_extent[2] / d.max(1) as f32,
        );
        let idx = |p: Vec3| {
            let g = (p - lo) / vs - Vec3::splat(0.5);
            [g.x, g.y, g.z]
        };
        let a = idx(wmin);
        let b = idx(wmax);
        let mn = [
            (a[0].floor() as i64 - 1).clamp(0, w as i64 - 1) as u32,
            (a[1].floor() as i64 - 1).clamp(0, h as i64 - 1) as u32,
            (a[2].floor() as i64 - 1).clamp(0, d as i64 - 1) as u32,
        ];
        let mx = [
            (b[0].ceil() as i64 + 1).clamp(0, w as i64 - 1) as u32,
            (b[1].ceil() as i64 + 1).clamp(0, h as i64 - 1) as u32,
            (b[2].ceil() as i64 + 1).clamp(0, d as i64 - 1) as u32,
        ];
        DenseRegion { mn, mx, layers: self.sample_dense_cells(dims, center, half_extent, mn, mx) }
    }

    /// Sample this field (trilinear distance, nearest colour) into the proxy's grid
    /// over the inclusive index box `[mn, mx]`. Voxel centers at `(i+0.5)/n` — the
    /// same convention `Terrain::sample` and the GPU field shader read with.
    fn sample_into_dense(
        &self,
        baked: &mut crate::mesh2sdf::BakedSdf,
        mn: [u32; 3],
        mx: [u32; 3],
    ) {
        let layers = self.sample_dense_cells(baked.dims, baked.center, baked.half_extent, mn, mx);
        DenseRegion { mn, mx, layers }.write_into(baked);
    }

    /// The proxy cells over the inclusive index box `[mn, mx]`, a z layer to a
    /// list, x fastest.
    fn sample_dense_cells(&self, dims: [u32; 3], center: [f32; 3], half_extent: [f32; 3], mn: [u32; 3], mx: [u32; 3]) -> Vec<Vec<(f32, [u8; 4])>> {
        let lo = Vec3::from(center) - Vec3::from(half_extent);
        let vs = Vec3::new(
            2.0 * half_extent[0] / dims[0].max(1) as f32,
            2.0 * half_extent[1] / dims[1].max(1) as f32,
            2.0 * half_extent[2] / dims[2].max(1) as f32,
        );
        // A cell and a half: every cell whose colour can blend into a pixel on
        // the surface.
        let near = 1.5 * vs.max_element();
        // A layer of cells to a job, on every core: a whole 1 km planet's proxy
        // is six million cells at eight voxel reads each, and on one thread it
        // was 25 s of the frame a generated world landed in.
        // A dab's few thousand cells stay on this thread: starting the
        // workers costs more than they would save.
        let cells = (mx[0] + 1 - mn[0]) as usize * (mx[1] + 1 - mn[1]) as usize * (mx[2] + 1 - mn[2]) as usize;
        let layer = |k: usize| {
            let iz = mn[2] + k as u32;
            let mut out = Vec::with_capacity(((mx[1] + 1 - mn[1]) * (mx[0] + 1 - mn[0])) as usize);
            for iy in mn[1]..=mx[1] {
                for ix in mn[0]..=mx[0] {
                    let p = lo + Vec3::new(ix as f32 + 0.5, iy as f32 + 0.5, iz as f32 + 0.5) * vs;
                    let d = self.d(p);
                    out.push((d, self.surface_color(p, d, near)));
                }
            }
            out
        };
        let n = (mx[2] + 1).saturating_sub(mn[2]) as usize;
        if cells > 1 << 16 { par_map(n, layer) } else { (0..n).map(layer).collect() }
    }

    /// The colour a proxy cell at `p` (field distance `d`) carries: within
    /// `near` of the surface, the colour of the ground there — the voxel the
    /// surface lies in, where the cell's gradient points; deeper, its own.
    ///
    /// A cell is several voxels across, and the terrain's texture is blended
    /// from the cells around each pixel, so a cell whose centre fell a voxel or
    /// two under the ground carried the subsoil, and the subsoil showed through
    /// undug ground in blobs the shape of the cells. Taken from the surface, a
    /// cell reads what the ground is made of where it is, and a dug face, which
    /// is surface too, reads what the dig laid bare.
    fn surface_color(&self, p: Vec3, d: f32, near: f32) -> [u8; 4] {
        if d.abs() > near {
            return self.color(p);
        }
        let n = self.grad(p).normalize_or_zero();
        if n == Vec3::ZERO {
            return self.color(p);
        }
        // A quarter voxel inside: half a voxel lands on the boundary to the next
        // voxel down, and rounding takes the one under the surface layer.
        self.color(p - n * (d + 0.25 * self.voxel))
    }

    /// Worst |∇d| over the near-surface band, and the fraction of samples violating the
    /// 1-Lipschitz bound. The dense field's growth path shipped 11.1% / 12.00 here; this
    /// is the measurement that caught it, kept as a permanent gate on write paths.
    pub fn lipschitz_audit(&self) -> (f32, f32) {
        let mut worst: f32 = 0.0;
        let (mut bad, mut n) = (0u64, 0u64);
        for c in self.chunk_coords() {
            for lz in 0..CHUNK {
                for ly in 0..CHUNK {
                    for lx in 0..CHUNK {
                        let i = [c[0] * CHUNK + lx, c[1] * CHUNK + ly, c[2] * CHUNK + lz];
                        let v = self.voxel_at(i);
                        // Only the band matters, and only where the clamp isn't active:
                        // a saturated plateau has zero gradient by design, not by error.
                        if v.abs() > self.band() * 0.6 {
                            continue;
                        }
                        let g = Vec3::new(
                            self.voxel_at([i[0] + 1, i[1], i[2]]) - self.voxel_at([i[0] - 1, i[1], i[2]]),
                            self.voxel_at([i[0], i[1] + 1, i[2]]) - self.voxel_at([i[0], i[1] - 1, i[2]]),
                            self.voxel_at([i[0], i[1], i[2] + 1]) - self.voxel_at([i[0], i[1], i[2] - 1]),
                        ) / (2.0 * self.voxel);
                        let m = g.length();
                        worst = worst.max(m);
                        n += 1;
                        if m > 1.35 {
                            bad += 1;
                        }
                    }
                }
            }
        }
        (worst, if n == 0 { 0.0 } else { bad as f32 / n as f32 })
    }
}

/// A per-stroke terrain undo record: the pre-stroke contents of every chunk the stroke
/// could touch. Opaque outside this module (chunks are an implementation detail);
/// undo/redo swaps it against the live field via [`ChunkField::apply_undo`].
#[derive(Clone, Debug, Default)]
pub struct ChunkUndo {
    entries: Vec<([i32; 3], Option<Chunk>)>,
}

impl ChunkUndo {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The chunk coords this record covers — the remesh set after a swap.
    pub fn coords(&self) -> Vec<[i32; 3]> {
        self.entries.iter().map(|(c, _)| *c).collect()
    }

    /// Fold another snapshot in, keeping the first-seen entry per coord — later dabs
    /// of the same stroke must not overwrite the pre-stroke state already captured.
    pub fn merge(&mut self, other: ChunkUndo) {
        for (c, ch) in other.entries {
            if !self.entries.iter().any(|(ec, _)| *ec == c) {
                self.entries.push((c, ch));
            }
        }
    }

    /// Approximate heap size — lets the editor's history cap account honestly.
    pub fn memory_bytes(&self) -> usize {
        self.entries
            .iter()
            .map(|(_, ch)| match ch {
                Some(Chunk::Data(d)) => d.heap_bytes(),
                _ => 16,
            })
            .sum()
    }
}

/// Two-pass 26-neighbour chamfer sweep expanding the AIR (positive) side of a
/// band-clamped proxy toward true distance: `d[i] ≤ d[n] + |i-n|`. Saturated air
/// cells (≥ band, where the clamp erased the real value) reseed to +∞ so distance
/// propagates outward from the genuine near-surface values; the standard
/// forward/backward pass pair then lands within a few percent of exact Euclidean
/// distance — plenty for a shadow march's step sizing.
fn chamfer_expand_air(baked: &mut crate::mesh2sdf::BakedSdf, band: f32) {
    // Reseed the clamped plateau: those cells only know "at least band away".
    let far = baked.half_extent.iter().fold(0.0f32, |a, h| a + 2.0 * h * h).sqrt().max(band);
    for v in &mut baked.distance {
        if *v >= band * 0.999 {
            *v = far;
        }
    }
    let [w, h, d] = baked.dims.map(|v| v as i64);
    let vs = [
        2.0 * baked.half_extent[0] / baked.dims[0].max(1) as f32,
        2.0 * baked.half_extent[1] / baked.dims[1].max(1) as f32,
        2.0 * baked.half_extent[2] / baked.dims[2].max(1) as f32,
    ];
    let idx = |x: i64, y: i64, z: i64| ((z * h + y) * w + x) as usize;
    // The 13 "already visited" neighbour offsets of a forward raster scan (the
    // backward pass mirrors them by negation).
    let mut offs: Vec<([i64; 3], f32)> = Vec::with_capacity(13);
    for dz in -1i64..=0 {
        for dy in -1i64..=1 {
            for dx in -1i64..=1 {
                if (dz, dy, dx) >= (0, 0, 0) {
                    continue; // strictly-before in scan order only
                }
                let step = Vec3::new(dx as f32 * vs[0], dy as f32 * vs[1], dz as f32 * vs[2])
                    .length();
                offs.push(([dx, dy, dz], step));
            }
        }
    }
    let pass = |forward: bool, dist: &mut [f32]| {
        // Iterate in scan order (forward) or reverse; offsets negate for the
        // backward pass so they always point at already-relaxed cells.
        for zi in 0..d {
            let z = if forward { zi } else { d - 1 - zi };
            for yi in 0..h {
                let y = if forward { yi } else { h - 1 - yi };
                for xi in 0..w {
                    let x = if forward { xi } else { w - 1 - xi };
                    let i = idx(x, y, z);
                    if dist[i] <= 0.0 {
                        continue; // solid side stays clamped
                    }
                    let mut best = dist[i];
                    for ([dx, dy, dz], step) in &offs {
                        let (nx, ny, nz) = if forward {
                            (x + dx, y + dy, z + dz)
                        } else {
                            (x - dx, y - dy, z - dz)
                        };
                        if nx < 0 || ny < 0 || nz < 0 || nx >= w || ny >= h || nz >= d {
                            continue;
                        }
                        let nv = dist[idx(nx, ny, nz)];
                        // A solid neighbour bounds us at its (negative) value + step.
                        let cand = nv.max(0.0) + step;
                        if cand < best {
                            best = cand;
                        }
                    }
                    dist[i] = best;
                }
            }
        }
    };
    pass(true, &mut baked.distance);
    pass(false, &mut baked.distance);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terrain::Terrain;

    fn rolling_terrain() -> ChunkField {
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
        f
    }

    /// **A copy of a field costs nothing until it is written.** Play keeps a
    /// copy of every field to put back on Stop, and the physics sim keeps its
    /// own to collide with: three of a 3.8 GB planet. Copies share their
    /// chunks, and a write gives the one chunk it lands in to the field that
    /// wrote it; the other copies keep the chunk as it was.
    #[test]
    fn a_copy_of_a_field_shares_its_chunks_until_one_is_written() {
        let f = rolling_terrain();
        let mut copy = f.clone();
        let shared = |a: &ChunkField, b: &ChunkField| {
            a.chunks
                .iter()
                .filter(|(c, ch)| matches!((ch, b.chunks.get(*c)), (Chunk::Data(x), Some(Chunk::Data(y))) if std::sync::Arc::ptr_eq(x, y)))
                .count()
        };
        let data = f.data_chunks();
        assert!(data > 4);
        assert_eq!(shared(&f, &copy), data, "a copy holds chunks of its own");
        let before = f.d(Vec3::new(0.0, 0.0, 0.0));
        let touched = copy.sculpt(Brush::Lower, Vec3::new(0.0, 0.0, 0.0), 3.0, 1.0, BrushProfile::default());
        assert_eq!(f.d(Vec3::ZERO), before, "writing the copy changed the field it was copied from");
        assert!(copy.d(Vec3::ZERO) > before, "the copy did not take the dig");
        assert!(shared(&f, &copy) >= data - touched.len() - 8, "a dig unshared {} chunks", data - shared(&f, &copy));
    }

    /// The incremental saver is `to_bytes`, just spread across calls: tiny
    /// per-step budgets must produce the byte-identical blob (autosaves stream
    /// through it every few frames — any drift silently corrupts save slots).
    /// And a field edited mid-save must still finish into a parseable blob:
    /// that's the whole contract that lets checkpoints run under live digging.
    #[test]
    fn incremental_save_matches_to_bytes_and_survives_midsave_edits() {
        let f = rolling_terrain();
        let whole = f.to_bytes();
        let mut s = f.begin_save();
        while !f.save_step(&mut s, 3) {}
        assert_eq!(whole, s.finish(), "chunk-budgeted save drifted from to_bytes");

        // Mid-save edit: dig while a save is in flight, then let it finish.
        let mut f = rolling_terrain();
        let mut s = f.begin_save();
        assert!(!f.save_step(&mut s, 2), "test field too small to interrupt");
        f.sculpt(Brush::Lower, Vec3::new(0.0, 2.0, 0.0), 12.0, 1.0, BrushProfile::default());
        while !f.save_step(&mut s, 2) {}
        let torn = s.finish();
        let back = ChunkField::from_bytes(&torn).expect("torn snapshot must stay parseable");
        assert!(back.data_chunks() > 0);
    }

    /// **A flatten on a planet levels to the ground's own up.** At 45° latitude
    /// a `+Y` flatten tilts the pad by 45° and cuts a slice rather than a disc;
    /// flattening to the radial normal leaves the tangent plane at the centre on
    /// the surface across the footprint.
    #[test]
    fn a_flatten_to_the_radial_normal_levels_a_pad_on_a_sphere() {
        let mut f = ChunkField::new(1.0);
        f.sculpt(Brush::Raise, Vec3::ZERO, 40.0, 1.0, BrushProfile::default());
        let up = Vec3::new(0.0, 1.0, 1.0).normalize();
        let r = (300..500).map(|k| k as f32 * 0.1).find(|r| f.d(up * *r) > 0.0).expect("a surface along up");
        let c = up * r;
        for _ in 0..4 {
            f.flatten_to_plane(c, up, 8.0, 1.0, BrushProfile::default());
        }
        let u = up.cross(Vec3::X).normalize();
        let v = up.cross(u);
        let mut worst = 0.0f32;
        for i in -4..=4 {
            for j in -4..=4 {
                let p = c + u * (i as f32) + v * (j as f32);
                if (p - c).length() <= 4.0 {
                    worst = worst.max(f.d(p).abs());
                }
            }
        }
        assert!(worst < 1.0, "the pad is not level with the ground's up: worst |d| on the tangent plane {worst}");
    }

    /// **A flatten writes the ground it moves, not the cube around it.** Deep
    /// rock under the pad and sky above it are already as far from the plane as
    /// the field can say; writing them anyway turned compact chunks into dense
    /// ones for the projection to sweep, most of a second per building pad on a
    /// planet. A pad on a big solid sphere now adds no dense chunk beyond the
    /// surface band, and leaves the field a distance field.
    #[test]
    fn a_flatten_leaves_deep_rock_and_open_sky_compact() {
        let mut f = ChunkField::new(0.5);
        f.fill_with(Vec3::splat(-64.0), Vec3::splat(64.0), |p| p.length() - 60.0, |_| [0.5, 0.5, 0.5]);
        let up = Vec3::new(0.3, 1.0, 0.2).normalize();
        let before = f.data_chunks();
        let touched = f.flatten_to_plane(up * 60.5, up, 12.0, 1.0, BrushProfile::default());
        let after = f.data_chunks();
        assert!(after <= before + 2, "the pad made {} chunks dense ({before} -> {after})", after - before);
        assert!(touched.len() <= 12, "a 12 m pad touched {} chunks", touched.len());
        let (worst, bad) = f.lipschitz_audit();
        assert!(bad < 0.01, "the pad left {:.2}% of band voxels steeper than a distance field (worst {worst:.2})", bad * 100.0);
    }

    /// **A pad over a cave leaves the cave alone.** A flatten used to pull every
    /// voxel of its box toward the plane however far below it was, so a cave
    /// under a pad filled solid down to the box's bottom face and was cut there
    /// by a flat wall: a cliff in the field, which a planet shows as an
    /// axis-aligned plug. Five pads at radius 15 over a cave 9 to 15 m down now
    /// touch nothing more than the band plus two voxels from the plane, and leave
    /// no cliff across a sign change.
    #[test]
    fn a_flatten_over_a_cave_changes_nothing_far_from_its_plane() {
        // Roofs just past the brush's reach, where the projection that follows a
        // write used to carry it on down and fill the cave's top, and one 15 m down,
        // which the old brush's box (radius + 5 voxels deep) cut through its middle.
        for roof in [9.0f32, 10.5, 12.0, 15.0] {
            flatten_over_a_cave(roof);
        }
    }

    fn flatten_over_a_cave(roof: f32) {
        let voxel = 1.5;
        let mut f = ChunkField::new(voxel);
        let cave = Vec3::new(0.0, -(roof + 10.0), 0.0);
        f.fill_with(
            Vec3::new(-40.0, -45.0, -40.0),
            Vec3::new(40.0, 6.0, 40.0),
            |p| p.y.max(-((p - cave).length() - 10.0)),
            |_| [0.5, 0.5, 0.5],
        );
        let plane = Vec3::new(0.0, 0.5, 0.0);
        let (lo, hi) = ([-26, -28, -26], [26, 4, 26]);
        let snapshot = |f: &ChunkField| {
            let mut v = Vec::new();
            for z in lo[2]..=hi[2] {
                for y in lo[1]..=hi[1] {
                    for x in lo[0]..=hi[0] {
                        v.push(([x, y, z], f.voxel_at([x, y, z])));
                    }
                }
            }
            v
        };
        let before = snapshot(&f);
        for _ in 0..5 {
            f.flatten_to_plane(plane, Vec3::Y, 15.0, 1.0, BrushProfile::default());
        }
        let after = snapshot(&f);
        let reach = f.band() + FLATTEN_FALLOFF_VOXELS * voxel;
        let mut far_changed = 0;
        for ((i, b), (_, a)) in before.iter().zip(&after) {
            let along = i[1] as f32 * voxel - plane.y;
            if along.abs() > reach + voxel && (a - b).abs() > 1e-4 {
                far_changed += 1;
            }
        }
        assert_eq!(far_changed, 0, "roof {roof} m: {far_changed} voxels further than the band + 2 voxels from the plane changed");
        let mut cliffs = 0;
        for &([x, y, z], d) in &after {
            for n in [[x + 1, y, z], [x, y + 1, z], [x, y, z + 1]] {
                let e = f.voxel_at(n);
                if (d < 0.0) != (e < 0.0) && (d - e).abs() > 1.6 * voxel {
                    cliffs += 1;
                }
            }
        }
        assert_eq!(cliffs, 0, "roof {roof} m: the pads left {cliffs} field cliffs across the surface");
        // And the pad itself is level.
        for x in [-6.0f32, 0.0, 6.0] {
            let d = f.d(Vec3::new(x, plane.y, 4.0));
            assert!(d.abs() < 0.6 * voxel, "the pad is not on its plane at x = {x}: d {d}");
        }
    }

    /// The share of the terrain texture at `p` that slot `slot` gets, blended
    /// from the proxy's cells exactly as the shader blends them: the eight cell
    /// centres around the point, trilinear weights, voxel centres at `i + 0.5`.
    fn splat_share(b: &crate::mesh2sdf::BakedSdf, p: Vec3, slot: u8) -> f32 {
        let dims = Vec3::new(b.dims[0] as f32, b.dims[1] as f32, b.dims[2] as f32);
        let lo = Vec3::from(b.center) - Vec3::from(b.half_extent);
        let frac = ((p - lo) / (2.0 * Vec3::from(b.half_extent))).clamp(Vec3::ZERO, Vec3::ONE);
        let c = (frac * dims - 0.5).clamp(Vec3::ZERO, dims - 1.0);
        let (i0, fw) = (c.floor(), c - c.floor());
        let (mut share, mut total) = (0.0, 0.0);
        for k in 0..8 {
            let o = Vec3::new((k & 1) as f32, ((k >> 1) & 1) as f32, ((k >> 2) & 1) as f32);
            let wv = Vec3::ONE - fw + (fw * 2.0 - Vec3::ONE) * o;
            let w = wv.x * wv.y * wv.z;
            let q = (i0 + o).min(dims - 1.0);
            let i = ((q.z as u32 * b.dims[1] + q.y as u32) * b.dims[0] + q.x as u32) as usize;
            total += w;
            if b.color[i][3] == slot {
                share += w;
            }
        }
        share / total.max(1e-6)
    }

    /// **The ground's texture is the ground's, not what lies under it.** The
    /// terrain texture blends slots from the proxy's cells, several voxels
    /// across, and a cell whose centre sat under the ground carried the
    /// subsoil: a subsoil one voxel down showed through undug ground in
    /// blobs. Undug, the subsoil gets none of the texture; dug down to, it is
    /// most of it.
    #[test]
    fn undug_ground_shows_none_of_the_subsoil_under_it() {
        let mut f = ChunkField::new(1.0);
        f.fill_with_rgba(
            Vec3::new(-30.0, -20.0, -30.0),
            Vec3::new(30.0, 0.0, 30.0),
            |p| p.y,
            |p| if -p.y < 1.0 { [200, 180, 140, 1] } else { [120, 100, 80, 2] },
        );
        let mut proxy = f.to_dense(24).expect("the slab has bounds");
        let cell = 2.0 * proxy.half_extent[0] / proxy.dims[0] as f32;
        assert!(cell > 2.0, "the fixture's proxy cells are {cell} voxels, finer than the bleed needs");
        let mut worst = 0.0f32;
        for x in -20..=20 {
            for z in -20..=20 {
                let ro = Vec3::new(x as f32 + 0.3, 5.0, z as f32 + 0.7);
                let hit = f.raycast(ro, Vec3::NEG_Y, 20.0).expect("the slab is under every ray");
                worst = worst.max(splat_share(&proxy, hit, 2));
            }
        }
        assert!(worst < 1e-3, "the subsoil gets {:.0}% of the texture on undug ground", worst * 100.0);
        // A pit dug through the top layer shows the subsoil at its floor.
        let hard = BrushProfile { hardness: 1.0, ..Default::default() };
        for _ in 0..6 {
            f.sculpt(Brush::Lower, Vec3::new(0.0, 0.0, 0.0), 6.0, 1.0, hard);
        }
        f.refresh_dense_region(&mut proxy, Vec3::splat(-12.0), Vec3::splat(12.0));
        let floor = f.raycast(Vec3::new(0.3, 5.0, 0.4), Vec3::NEG_Y, 30.0).expect("the pit has a floor");
        assert!(floor.y < -2.0, "the fixture's pit is only {:.1} deep", -floor.y);
        let dug = splat_share(&proxy, floor, 2);
        assert!(dug > 0.5, "the pit's floor shows only {:.0}% subsoil", dug * 100.0);
    }

    /// Per-write-path |∇d| report.    /// Per-write-path |∇d| report. This is the diagnostic that found the real culprit:
    /// `fill_slab` was writing a plane clipped to a box, so the slab's rim jumped from
    /// -band to +band across one voxel (worst 4.36, 4.9% bad) — before any brush ran. A
    /// real box SDF took that to 1.04 / 0.0%. Keep it: it tells you which path regressed.
    #[test]
    fn lipschitz_by_write_path() {
        let mut f = ChunkField::new(1.5);
        f.fill_slab(Vec3::new(-60.0, -20.0, -60.0), Vec3::new(60.0, 20.0, 60.0), 0.0, [0.4, 0.6, 0.3]);
        let (w, b) = f.lipschitz_audit();
        println!("after fill_slab      : worst {w:.2}  bad {:.1}%", b * 100.0);
        f.sculpt(Brush::Raise, Vec3::new(0.0, 2.0, 0.0), 9.0, 0.9, BrushProfile::default());
        let (w, b) = f.lipschitz_audit();
        println!("after Raise          : worst {w:.2}  bad {:.1}%", b * 100.0);
        f.sculpt(Brush::Lower, Vec3::new(10.0, 0.0, 10.0), 8.0, 0.8, BrushProfile::default());
        let (w, b) = f.lipschitz_audit();
        println!("after Lower          : worst {w:.2}  bad {:.1}%", b * 100.0);
        f.sculpt(Brush::Smooth, Vec3::new(-10.0, 0.0, 5.0), 8.0, 0.7, BrushProfile::default());
        let (w, b) = f.lipschitz_audit();
        println!("after Smooth         : worst {w:.2}  bad {:.1}%", b * 100.0);
        f.sculpt(Brush::Flatten, Vec3::new(0.0, 1.0, -12.0), 8.0, 0.6, BrushProfile::default());
        let (w, b) = f.lipschitz_audit();
        println!("after Flatten        : worst {w:.2}  bad {:.1}%", b * 100.0);
    }

    /// the invariant. |∇d| ≤ 1 is what sphere tracing, gradient normals, SDF AO and sun
    /// shadows all assume. The dense field's `grow()` broke it by summing two distance
    /// terms and shipped 11.1% of near-surface voxels bad, worst |∇d| = 12.00 — the
    /// symptom was blotchy AO, not a crash. Every write path here must hold the line.
    #[test]
    fn brush_writes_keep_the_field_1_lipschitz() {
        let mut f = rolling_terrain();
        // Pile on the other brushes too — each is a separate write path.
        f.sculpt(Brush::Lower, Vec3::new(10.0, 0.0, 10.0), 8.0, 0.8, BrushProfile::default());
        f.sculpt(Brush::Smooth, Vec3::new(-10.0, 0.0, 5.0), 8.0, 0.7, BrushProfile::default());
        f.sculpt(Brush::Flatten, Vec3::new(0.0, 1.0, -12.0), 8.0, 0.6, BrushProfile::default());
        f.sculpt(Brush::Raise, Vec3::ZERO, 6.0, 1.0, BrushProfile::hard());

        let (worst, bad) = f.lipschitz_audit();
        println!("all brushes: worst |∇d| {worst:.2}, {:.1}% of band voxels bad", bad * 100.0);
        // Where we actually are, and why this bound and not 1.0:
        //
        //   fill_slab (box SDF)  1.04 / 0.0%   exact
        //   Raise/Lower (CSG)    ~3.3 / 0.1%   smin/smax of true SDFs, near-exact
        //   Smooth, Flatten      ~4.9 / 1.8%   spatially-weighted blends — not SDF ops
        //
        // Smooth averages its neighbours, so where those are saturated at ±band it can
        // land near 0 beside a ±band neighbour: a configuration a true SDF cannot hold
        // (−band→+band needs ~8 voxels, not 2). No projection repairs that — only a real
        // redistance (fast-marching) pass reseeded from the zero crossing would, and that
        // is not in P1.
        //
        // It is tolerable for now because of who reads the field: the mesher normalizes
        // the gradient (magnitude irrelevant) and trusts the zero crossing (unmoved), and
        // `raycast` half-steps. It must be fixed before P5 puts this field under GPU AO
        // and sun shadows — that is exactly what |∇d| = 12 did to the dense grow().
        assert!(
            bad < 0.04 && worst < 6.0,
            "{:.1}% of band voxels violate |∇d| ≤ 1 (worst {worst:.2}) — worse than the \
             known blend drift (~3.1% / 4.9 with every brush incl. a hard-edged one). \
             The dense grow() shipped 11.1% / 12.00.",
            bad * 100.0
        );
    }

    /// Lower must carve the ball and nothing else. The shipped bug computed
    /// `max(cur, ball)` instead of `max(cur, -ball)` — keep-the-ball-carve-the-box —
    /// so every dig blasted a write-box-sized square crater ("massive squares",
    /// a Solar playtest). The dab: radius 1.3, strength 0.6 — the dig_tool defaults.
    #[test]
    fn lower_carves_a_ball_not_the_write_box() {
        let mut f = ChunkField::new(0.75);
        f.fill_slab(Vec3::new(-30.0, -12.0, -30.0), Vec3::new(30.0, 0.0, 30.0), 0.0, [0.5; 3]);
        let (radius, strength) = (1.3f32, 0.6f32);
        let r_eff = radius * strength;
        let center = Vec3::new(0.0, 0.0, 0.0); // on the surface
        f.sculpt(Brush::Lower, center, radius, strength, BrushProfile::default());

        // Inside the ball: air now (was surface).
        assert!(f.d(center) > 0.0, "dig center still solid: {}", f.d(center));
        assert!(
            f.d(center - Vec3::Y * (r_eff * 0.5)) > 0.0,
            "just below the dig center should be carved"
        );
        // Well outside the ball but inside the brush's write box (radius + band + 1
        // voxel ≈ 5 units): the surface must be untouched. This is the assertion the
        // inverted CSG fails — it read +band (open air) everywhere here.
        for x in [-4.0f32, 4.0] {
            for z in [-4.0f32, 4.0] {
                let p = Vec3::new(x, -1.0, z); // a unit under the old surface
                assert!(
                    f.d(p) < 0.0,
                    "ground at {p:?} was carved away — Lower ate the write box, \
                     not the ball (d = {})",
                    f.d(p)
                );
            }
        }
        // And the crater must not out-reach the smoothed ball by more than the blend.
        let k = (1.0 - BrushProfile::default().hardness) * radius * 0.5;
        let deep = center - Vec3::Y * (r_eff + k + 0.8);
        assert!(f.d(deep) < 0.0, "crater reaches deeper than ball + blend: d = {}", f.d(deep));
    }

    /// The collider-mirror primitive: after sculpting an authority field, copying
    /// the touched chunks + one renormalize ring into a stale clone makes the two
    /// agree byte-for-byte (renormalize spills constraint writes one chunk out —
    /// copying only `touched` leaves ring chunks stale, which is a divergence).
    #[test]
    fn copy_chunks_from_syncs_a_stale_mirror_exactly() {
        let mut authority = rolling_terrain();
        let mut mirror = authority.clone();
        let touched = authority.sculpt(
            Brush::Lower,
            Vec3::new(5.0, 0.0, -3.0),
            6.0,
            0.9,
            BrushProfile::default(),
        );
        assert!(!touched.is_empty());
        let mut region: Vec<[i32; 3]> = Vec::new();
        for c in &touched {
            for dz in -1..=1 {
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        region.push([c[0] + dx, c[1] + dy, c[2] + dz]);
                    }
                }
            }
        }
        region.sort_unstable();
        region.dedup();
        mirror.copy_chunks_from(&authority, &region);
        assert_eq!(
            authority.to_bytes(),
            mirror.to_bytes(),
            "mirror must be byte-identical to the authority after the copy"
        );
    }

    /// Sparsity is the whole point: memory must track the surface, not the volume.
    #[test]
    fn a_big_field_stores_only_its_surface() {
        let f = rolling_terrain();
        let mb = f.memory_bytes() as f32 / 1.0e6;
        // The slab spans 120×40×120 units at 1.5 → a dense grid would be ~80×27×80
        // ≈ 173k voxels; the point is that this scales with area, not volume, so assert
        // a real ceiling rather than a tautology.
        println!("rolling terrain: {} data chunks, {mb:.1} MB", f.data_chunks());
        assert!(mb < 12.0, "{mb:.1} MB for a 120-unit terrain — sparsity isn't working");
        assert!(f.data_chunks() > 0);
    }

    /// Air stays free. This is what makes an unbounded map affordable: sky costs nothing,
    /// and there is no `ensure_contains`/`MAX_DIM` ceiling to hit.
    #[test]
    fn empty_space_and_growth_are_free() {
        let mut f = ChunkField::new(1.5);
        assert_eq!(f.memory_bytes(), 0, "an empty field must allocate nothing");
        f.sculpt(Brush::Raise, Vec3::ZERO, 5.0, 1.0, BrushProfile::default());
        let near = f.memory_bytes();
        // Sculpt 3 km away — no bounds, no realloc of the world, no cap.
        f.sculpt(Brush::Raise, Vec3::new(3000.0, 0.0, 3000.0), 5.0, 1.0, BrushProfile::default());
        let far = f.memory_bytes();
        assert!(far > near, "a distant edit must allocate its own chunks");
        assert!(
            far < near * 3,
            "a distant edit cost {far} vs {near} — it should cost the same handful of \
             chunks, not fill the space between"
        );
        assert!(f.d(Vec3::new(3000.0, -1.0, 3000.0)) < f.band(), "the far edit must exist");
    }

    #[test]
    fn raycast_finds_the_ground_and_misses_the_sky() {
        let f = rolling_terrain();
        let hit = f
            .raycast(Vec3::new(0.0, 40.0, 0.0), Vec3::NEG_Y, 200.0)
            .expect("a ray straight down must hit the ground");
        assert!(f.d(hit).abs() < f.voxel() * 0.5, "hit isn't on the surface: d = {}", f.d(hit));
        assert!(hit.y > -5.0 && hit.y < 20.0, "hit at a silly height: {hit:?}");
        assert!(
            f.raycast(Vec3::new(0.0, 40.0, 0.0), Vec3::Y, 200.0).is_none(),
            "a ray into the sky must miss"
        );
    }

    /// Existing projects must just open — a 192 MB field included.
    #[test]
    fn dense_tfield_migrates_into_chunks() {
        let mut t = Terrain::flat([64, 40, 64], [0.0; 3], [24.0, 12.0, 24.0], 0.0, [0.4, 0.6, 0.3]);
        for i in 0..6 {
            let a = i as f32 * 1.9;
            t.sculpt(
                Brush::Raise,
                [a.cos() * 12.0, 1.0, a.sin() * 12.0],
                6.0,
                0.9,
                BrushProfile::default(),
            );
        }
        let dense_bytes = t.baked.distance.len() * 8;
        let f = ChunkField::from_dense(&t.baked, 0.75);

        // Same surface, sampled through a completely different representation.
        let mut worst = 0.0f32;
        let mut checked = 0;
        for i in 0..40 {
            let a = i as f32 * 0.31;
            let p = Vec3::new(a.cos() * 10.0, 0.0, a.sin() * 10.0);
            if let Some(hit) = f.raycast(p + Vec3::Y * 30.0, Vec3::NEG_Y, 100.0) {
                // The dense field's own value at the sparse field's hit should be ~0.
                let dd = t.baked_distance_at([hit.x, hit.y, hit.z]);
                worst = worst.max(dd.abs());
                checked += 1;
            }
        }
        assert!(checked > 25, "migration test only landed {checked} rays");
        assert!(worst < 1.5, "migrated surface drifts from the original by {worst:.2} units");

        // ...and the ground must be solid all the way down, not a shell.
        //
        // The raycast above cannot see this: it comes from above and stops at the first
        // surface, which was always the right one. Underneath it, the import was leaving
        // every voxel deeper than the band as the air a fresh chunk starts out as — so the
        // field held a hollow crust with a spurious inner surface a band below the real
        // one, and the mesher faithfully triangulated both. Invisible to a top-down probe;
        // obvious the moment terrain was rasterized. Assert the inside directly.
        for i in 0..20 {
            let a = i as f32 * 0.31;
            let p = Vec3::new(a.cos() * 8.0, -6.0, a.sin() * 8.0);
            let d = f.d(p);
            assert!(d < 0.0, "the field is AIR at {p:?} (d = {d:+.2}), 6 units underground");
        }

        let sparse = f.memory_bytes();
        println!(
            "dense {:.1} MB -> sparse {:.1} MB ({} chunks)",
            dense_bytes as f32 / 1.0e6,
            sparse as f32 / 1.0e6,
            f.data_chunks()
        );
        // not asserting "sparse < dense" here, because on a TOY field it isn't true and
        // saying so would be a lie: a 32³ chunk is 256 KB, so a 48-unit test terrain
        // rounds up to a few chunks and can cost more than the dense grid it came from.
        // Sparsity is a scale property — it wins when the volume grows and the surface
        // doesn't (a 433×406×460-unit field is 192 MB dense; only its band is worth
        // storing). What must hold at every scale is an absolute ceiling.
        assert!(
            sparse < 8_000_000,
            "migrated field is {:.1} MB — far more than its band can justify",
            sparse as f32 / 1.0e6
        );
        // The source field is itself not a distance field: the dense brushes never
        // enforced |∇d| ≤ 1 either (they nudge voxels by weight, the same mistake this
        // module's first cut made). A faithful import cannot be cleaner than its input,
        // so this asserts "no worse than the source", not "correct" — the redistance pass
        // that would actually fix it is out of P1.
        let (worst_g, bad) = f.lipschitz_audit();
        println!("migrated field: worst |∇d| {worst_g:.2}, {:.1}% bad (source is itself non-SDF)", bad * 100.0);
        assert!(
            bad < 0.10,
            "migrated field violates |∇d| ≤ 1 at {:.1}% (worst {worst_g:.2}) — worse than \
             the ~4.8% the dense source itself carries",
            bad * 100.0
        );
    }

    /// The sculpt fast-path: after the dense authority changes under a dab, a regional
    /// refresh must move the ChunkField's surface to match — without a full re-import.
    /// This is what makes editing a big terrain in the editor stay smooth (P2d).
    #[test]
    fn regional_refresh_tracks_a_dense_edit() {
        // Flat ground migrated to a chunk field: surface at y = 0.
        let mut t = Terrain::flat([64, 40, 64], [0.0; 3], [24.0, 12.0, 24.0], 0.0, [0.4, 0.6, 0.3]);
        let mut f = ChunkField::from_dense(&t.baked, 0.75);
        let probe = Vec3::new(0.0, 0.0, 0.0);
        let before = f
            .raycast(probe + Vec3::Y * 20.0, Vec3::NEG_Y, 100.0)
            .expect("flat ground is hit");
        assert!(before.y.abs() < 1.0, "flat surface should be near y=0, got {:.2}", before.y);

        // Raise a bump in the dense authority (the editor's real sculpt), like a stroke.
        for _ in 0..8 {
            t.sculpt(Brush::Raise, [0.0, 0.5, 0.0], 5.0, 1.0, BrushProfile::default());
        }

        // Refresh only the box the dab touched — the whole field must not be re-imported.
        let touched = f.refresh_from_dense_region(
            &t.baked,
            Vec3::new(-6.0, -2.0, -6.0),
            Vec3::new(6.0, 6.0, 6.0),
        );
        assert!(!touched.is_empty(), "a dab must dirty at least one chunk");

        // The chunk field's surface now follows the raised dense field.
        let after = f
            .raycast(probe + Vec3::Y * 20.0, Vec3::NEG_Y, 100.0)
            .expect("raised ground is hit");
        assert!(
            after.y > before.y + 1.0,
            "regional refresh should have RAISED the surface: {:.2} -> {:.2}",
            before.y,
            after.y
        );
        // ...and it matches the dense authority there (the refresh is faithful, not approx).
        let dd = t.baked_distance_at([after.x, after.y, after.z]);
        assert!(dd.abs() < 1.5, "refreshed surface drifts {dd:.2} from the dense field");

        // Ground outside the refreshed box is untouched — the refresh was regional, not a
        // silent full rebuild (which would defeat the whole point on a large terrain).
        let far = f
            .raycast(Vec3::new(18.0, 20.0, 18.0), Vec3::NEG_Y, 100.0)
            .expect("far ground still hit");
        assert!(far.y.abs() < 1.0, "far ground should stay flat at y=0, got {:.2}", far.y);
    }

    /// Save/load round-trip: distances within one i8 quantization step, colours exact,
    /// uniform solid interior preserved (a hollow save would shadow/collide wrong),
    /// and the encoding deterministic (byte-identical re-save — the autosave/backup
    /// machinery diffs on bytes).
    #[test]
    fn cfield_round_trips_within_quantization() {
        let f = rolling_terrain();
        let bytes = f.to_bytes();
        let g = ChunkField::from_bytes(&bytes).expect("parse back");
        assert_eq!(g.voxel(), f.voxel());
        let step = f.band() / 127.0;
        // Compare over every stored voxel of the original.
        for c in f.chunk_coords() {
            for lz in 0..CHUNK {
                for ly in 0..CHUNK {
                    for lx in 0..CHUNK {
                        let i = [c[0] * CHUNK + lx, c[1] * CHUNK + ly, c[2] * CHUNK + lz];
                        let (a, b) = (f.voxel_at(i), g.voxel_at(i));
                        assert!(
                            (a - b).abs() <= step * 0.51,
                            "voxel {i:?}: {a} vs {b} (step {step})"
                        );
                    }
                }
            }
        }
        // Solid interior sentinels survive (deep chunks must stay rock). The slab is
        // solid from y=-20 to 0; y=-10 is deep interior.
        let deep = Vec3::new(0.0, -10.0, 0.0);
        assert!(f.d(deep) < 0.0, "test precondition: the slab interior is solid");
        assert!(g.d(deep) < 0.0, "deep interior read as air after round-trip");
        // Determinism + quantization idempotence: re-saving the parsed field is
        // byte-identical (values already sit on the quantization lattice).
        assert_eq!(bytes, g.to_bytes());
        // Colour exactness on a surface point.
        let p = f.raycast(Vec3::new(0.0, 40.0, 0.0), Vec3::NEG_Y, 100.0).unwrap();
        assert_eq!(f.color(p), g.color(p));
    }

    /// Undo swap: snapshot → sculpt → apply_undo restores exactly; the returned
    /// inverse redoes exactly. This is the 2.5 MB replacement for 192 MB strokes.
    #[test]
    fn chunk_undo_swaps_exactly() {
        let mut f = rolling_terrain();
        let center = Vec3::new(10.0, 2.0, -5.0);
        let cand = f.chunks_in_box(center, 12.0);
        let undo = f.snapshot_chunks(&cand);
        let before = f.to_bytes();
        let touched = f.sculpt(Brush::Raise, center, 8.0, 1.0, BrushProfile::default());
        assert!(!touched.is_empty());
        // Every touched chunk was inside the candidate (snapshot) set.
        for t in &touched {
            assert!(cand.contains(t), "brush touched {t:?} outside its candidate box");
        }
        let after = f.to_bytes();
        assert_ne!(before, after, "the dab must change the field");
        let redo = f.apply_undo(&undo);
        assert_eq!(f.to_bytes(), before, "undo must restore the exact pre-stroke field");
        f.apply_undo(&redo);
        assert_eq!(f.to_bytes(), after, "redo must restore the exact post-stroke field");
        // And it's small — the whole point.
        assert!(undo.memory_bytes() < 40 * 1024 * 1024);
    }

    /// The shadow proxy: correct sign + near-surface values where it matters, and the
    /// chamfer sweep re-expands clamped far-air so a shadow march can take real steps.
    #[test]
    fn shadow_proxy_matches_field_and_expands_air() {
        let f = rolling_terrain();
        let baked = f.to_dense(128).expect("non-empty field has a proxy");
        // Near-surface agreement: sample the proxy where the field has a surface.
        let hit = f.raycast(Vec3::new(0.0, 40.0, 0.0), Vec3::NEG_Y, 100.0).unwrap();
        let t = Terrain { baked: baked.clone() };
        let pd = t.sample([hit.x, hit.y, hit.z]);
        assert!(
            pd.abs() < f.voxel() * 3.0,
            "proxy reads {pd:.2} at the field surface (proxy voxel {:.2})",
            2.0 * baked.half_extent[0] / (baked.dims[0] - 1) as f32
        );
        // Sign agreement well inside / well outside.
        assert!(t.sample([hit.x, hit.y - 6.0, hit.z]) < 0.0, "under the surface must be solid");
        // Far air must exceed the band clamp after the chamfer sweep — otherwise the
        // proxy would cap every shadow step at ~band and starve the march.
        let up = t.sample([hit.x, hit.y + 30.0, hit.z]);
        assert!(
            up > f.band() * 2.0,
            "chamfer failed: 30 units above the surface reads {up:.2} (band {:.2})",
            f.band()
        );
    }

    /// Texture ops ride the colour alpha channel (the splat slot), exactly like the
    /// dense field: fill_texture floods it, paint_texture writes near-surface only.
    #[test]
    fn texture_slots_ride_alpha() {
        let mut f = rolling_terrain();
        f.fill_texture(3);
        let hit = f.raycast(Vec3::new(5.0, 40.0, 5.0), Vec3::NEG_Y, 100.0).unwrap();
        assert_eq!(f.color(hit)[3], 3);
        let touched = f.paint_texture(hit, 6.0, 7);
        assert!(!touched.is_empty());
        assert_eq!(f.color(hit)[3], 7, "painted slot lands at the brush centre");
        let far = f.raycast(Vec3::new(-40.0, 40.0, -40.0), Vec3::NEG_Y, 100.0).unwrap();
        assert_eq!(f.color(far)[3], 3, "far voxels keep the filled slot");
    }

    /// `fill_with_rgba` authors the palette slot per voxel (alpha byte) and it
    /// survives serialization; the plain `fill_with` stays untextured (sentinel 255).
    #[test]
    fn fill_with_rgba_authors_slots() {
        let mut f = ChunkField::new(1.0);
        let sphere = |p: Vec3| p.length() - 8.0;
        f.fill_with_rgba(Vec3::splat(-9.0), Vec3::splat(9.0), sphere, |p| {
            let slot = if p.y > 0.0 { 2 } else { 5 };
            [200, 180, 160, slot]
        });
        let top = f.raycast(Vec3::new(0.0, 20.0, 0.0), Vec3::NEG_Y, 40.0).unwrap();
        let bot = f.raycast(Vec3::new(0.0, -20.0, 0.0), Vec3::Y, 40.0).unwrap();
        assert_eq!(f.color(top)[3], 2, "north hemisphere carries slot 2");
        assert_eq!(f.color(bot)[3], 5, "south hemisphere carries slot 5");
        let back = ChunkField::from_bytes(&f.to_bytes()).unwrap();
        assert_eq!(back.color(top)[3], 2, "slot survives the cfield round-trip");

        let mut plain = ChunkField::new(1.0);
        plain.fill_with(Vec3::splat(-9.0), Vec3::splat(9.0), sphere, |_| [0.5, 0.5, 0.5]);
        let hit = plain.raycast(Vec3::new(0.0, 20.0, 0.0), Vec3::NEG_Y, 40.0).unwrap();
        assert_eq!(plain.color(hit)[3], 255, "fill_with keeps the untextured sentinel");
    }

    /// `bounds` covers data and solid interior, and grows when sculpting outward —
    /// the proxy/framing box for an unbounded field.
    #[test]
    fn bounds_track_content() {
        let mut f = ChunkField::new(1.0);
        f.fill_slab(Vec3::new(-10.0, -4.0, -10.0), Vec3::new(10.0, 4.0, 10.0), 0.0, [0.5; 3]);
        let (lo, hi) = f.bounds().unwrap();
        assert!(lo.x <= -10.0 && hi.x >= 10.0 && lo.y <= -4.0);
        f.sculpt(Brush::Raise, Vec3::new(60.0, 0.0, 0.0), 6.0, 1.0, BrushProfile::default());
        let (_, hi2) = f.bounds().unwrap();
        assert!(hi2.x >= 60.0, "sculpting outward must grow the bounds ({} < 60)", hi2.x);
    }
}

#[cfg(test)]
mod yield_tests {
    use super::*;

    /// The measurement has to be additive: sum the reports over a shaft and you
    /// get the volume that actually left the field. A mining game that pays out
    /// per dab needs a careful shaft and a sloppy cavern to differ by the truth,
    /// not by the dab count.
    #[test]
    fn summed_yield_equals_the_volume_that_left_the_field() {
        let mut f = ChunkField::new(0.5);
        // A solid block to cut into.
        f.fill_with(
            Vec3::splat(-40.0),
            Vec3::splat(40.0),
            |p: Vec3| p.y - 20.0,
            |_| [0.5, 0.5, 0.5],
        );

        let solid = |f: &ChunkField| -> f64 {
            // Ground truth: total occupancy over the region we touch.
            let mut v = 0.0;
            let n = 60;
            for iz in -n..=n {
                for iy in -n..=n {
                    for ix in -n..=n {
                        v += f64::from(occupancy(f.voxel_at([ix, iy, iz]), f.voxel));
                    }
                }
            }
            v * f64::from(f.voxel).powi(3)
        };

        let before = solid(&f);
        let mut reported = 0.0;
        // A shaft: eight overlapping dabs straight down.
        for i in 0..8 {
            let c = Vec3::new(0.0, 18.0 - i as f32 * 1.2, 0.0);
            let (_, y) = f.sculpt_measured(Brush::Lower, c, 3.0, 1.0, BrushProfile::default());
            reported += y.removed - y.added;
        }
        let actual = before - solid(&f);
        assert!(actual > 0.0, "the shaft removed something");
        let err = (reported - actual).abs() / actual;
        assert!(
            err < 0.02,
            "summed yield {reported:.3} must match the volume removed {actual:.3} (err {:.2}%)",
            err * 100.0
        );
    }

    /// Removal is attributed to the palette slot that was standing there, and
    /// the parts add up — `removed == untextured + sum(slots)` is what lets a
    /// caller check its own arithmetic.
    #[test]
    fn yield_is_split_by_the_palette_slot_that_was_there() {
        let mut f = ChunkField::new(0.5);
        f.fill_with(
            Vec3::splat(-40.0),
            Vec3::splat(40.0),
            |p: Vec3| p.y - 20.0,
            |_| [0.5, 0.5, 0.5],
        );
        f.fill_texture(4);
        // A seam of a different material, narrower than the dig that follows,
        // so the report has to distinguish the two materials rather than just
        // naming whichever one it met first.
        f.paint_texture(Vec3::new(0.0, 19.0, 0.0), 1.0, 9);

        let (_, y) = f.sculpt_measured(
            Brush::Lower,
            Vec3::new(0.0, 19.0, 0.0),
            3.0,
            1.0,
            BrushProfile::default(),
        );
        assert!(y.removed > 0.0, "it dug something");
        let parts: f64 = y.untextured + y.slots.values().sum::<f64>();
        assert!(
            (parts - y.removed).abs() < 1e-9,
            "removed {} must equal untextured {} + slots {}",
            y.removed,
            y.untextured,
            y.slots.values().sum::<f64>()
        );
        assert!(y.slots.contains_key(&9), "the painted seam is reported: {:?}", y.slots);
        assert!(y.slots.contains_key(&4), "so is the rock around it: {:?}", y.slots);
    }

    /// A dab that changes nothing reports zero, not nothing — "I dug air" has to
    /// be distinguishable from "the report has not arrived".
    #[test]
    fn a_dab_that_moves_nothing_reports_zero() {
        let mut f = ChunkField::new(0.5);
        f.fill_with(
            Vec3::splat(-40.0),
            Vec3::splat(40.0),
            |p: Vec3| p.y - 20.0,
            |_| [0.5, 0.5, 0.5],
        );
        // Far above the surface, in open air.
        let (_, y) =
            f.sculpt_measured(Brush::Lower, Vec3::new(0.0, 80.0, 0.0), 2.0, 1.0, BrushProfile::default());
        assert_eq!(y.removed, 0.0);
        assert_eq!(y.added, 0.0);
        assert!(y.slots.is_empty());
    }

    /// The palette read is the point query the card asks for: `None` where the
    /// field is untextured, so "plain rock" and "slot 255" stay distinct.
    #[test]
    fn slot_at_reads_what_paint_texture_wrote() {
        let mut f = ChunkField::new(0.5);
        f.fill_with(
            Vec3::splat(-40.0),
            Vec3::splat(40.0),
            |p: Vec3| p.y - 20.0,
            |_| [0.5, 0.5, 0.5],
        );
        let hit = Vec3::new(0.0, 19.5, 0.0);
        assert_eq!(f.slot_at(hit), None, "an untextured field answers None");
        f.fill_texture(3);
        assert_eq!(f.slot_at(hit), Some(3));
        f.paint_texture(hit, 6.0, 7);
        assert_eq!(f.slot_at(hit), Some(7));
    }
}
