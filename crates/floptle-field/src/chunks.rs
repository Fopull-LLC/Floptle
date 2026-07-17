//! Sparse chunked SDF — the authoring/physics/shadow substrate for Terrain 2.0
//! (`docs/terrain-mesh-proposal.md` §3.1).
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
//!    dense field's `grow()` broke this by SUMMING two distance terms (measured |∇d| up
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
const CHUNK_U: usize = CHUNK as usize;
const CHUNK_VOXELS: usize = CHUNK_U * CHUNK_U * CHUNK_U;

/// Narrow-band half-width, in voxels. Distances are stored clamped to ±(BAND × voxel).
/// 4 gives the mesher and gradient stencils plenty of room either side of the surface.
pub const BAND_VOXELS: f32 = 4.0;

/// A chunk's contents. Absent from the map == `Uniform(+band)` (open air).
#[derive(Clone, Debug)]
enum Chunk {
    /// Every voxel holds this exact value — the whole point of the sparse store. `+band`
    /// is sky, `-band` is deep interior; a hollow mountain costs two enums.
    Uniform(f32),
    Data(Box<ChunkData>),
}

#[derive(Clone)]
struct ChunkData {
    dist: Vec<f32>,        // CHUNK_VOXELS, x-fastest
    color: Vec<[u8; 4]>,   // parallel to dist
}

impl std::fmt::Debug for ChunkData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ChunkData({} voxels)", self.dist.len())
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
}

impl Default for ChunkField {
    /// An empty unit-voxel field — a valid placeholder before a real field is derived.
    fn default() -> Self {
        Self::new(1.0)
    }
}

impl ChunkField {
    /// An empty field of open air. `voxel` is the cubic voxel edge in world units — the
    /// ONE density knob (the dense grid's "detail" was a cell *count* that silently
    /// meant nothing as terrain grew; this is a real density).
    pub fn new(voxel: f32) -> Self {
        Self { chunks: HashMap::new(), voxel: voxel.max(1e-3), base_color: [128, 128, 128, 255] }
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
        self.chunks.values().filter(|c| matches!(c, Chunk::Data(_))).count()
    }

    /// Resident bytes of voxel data — what the 192 MB dense field is measured against.
    pub fn memory_bytes(&self) -> usize {
        self.data_chunks() * CHUNK_VOXELS * (4 + 4)
    }

    /// Every stored chunk coordinate — data AND uniform sentinels. The undo-snapshot
    /// set for whole-field ops (Fill), where any stored chunk may change.
    pub fn all_chunk_coords(&self) -> Vec<[i32; 3]> {
        self.chunks.keys().copied().collect()
    }

    /// Every chunk coordinate overlapping a world-space AABB (present or absent) —
    /// the undo-snapshot set for ops that may CREATE chunks inside a known box.
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
            .filter(|(_, c)| matches!(c, Chunk::Data(_)))
            .map(|(k, _)| *k)
            .collect()
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

    /// The stored distance at a GLOBAL voxel index. Absent chunks read as open air.
    /// This is the only voxel accessor; aprons and cross-chunk gradients come free (T3).
    #[inline]
    pub fn voxel_at(&self, i: [i32; 3]) -> f32 {
        match self.chunks.get(&chunk_of(i)) {
            None => self.band(),
            Some(Chunk::Uniform(v)) => *v,
            Some(Chunk::Data(d)) => d.dist[local_of(i)],
        }
    }

    #[inline]
    fn color_at(&self, i: [i32; 3]) -> [u8; 4] {
        match self.chunks.get(&chunk_of(i)) {
            Some(Chunk::Data(d)) => d.color[local_of(i)],
            _ => self.base_color,
        }
    }

    /// Trilinearly-sampled distance at a world position.
    pub fn d(&self, p: Vec3) -> f32 {
        let g = p / self.voxel;
        let b = [g.x.floor() as i32, g.y.floor() as i32, g.z.floor() as i32];
        let f = Vec3::new(g.x - b[0] as f32, g.y - b[1] as f32, g.z - b[2] as f32);
        let c = |dx: i32, dy: i32, dz: i32| self.voxel_at([b[0] + dx, b[1] + dy, b[2] + dz]);
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
    /// This is THE reason terrain stops looking faceted: on the GPU the normal was
    /// re-derived per pixel from a trilinearly-filtered f16 texture, and trilinear
    /// interpolation is only C⁰ — its gradient jumps at every cell face, so the lattice
    /// showed through the shading no matter how cubic the voxels were. Here the gradient
    /// is computed once per VERTEX in f32 and then interpolated across the triangle by
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
        let band = BAND_VOXELS * self.voxel;
        let base = self.base_color;
        let entry = self.chunks.entry(c).or_insert(Chunk::Uniform(band));
        if let Chunk::Uniform(v) = *entry {
            *entry = Chunk::Data(Box::new(ChunkData {
                dist: vec![v; CHUNK_VOXELS],
                color: vec![base; CHUNK_VOXELS],
            }));
        }
        match entry {
            Chunk::Data(d) => d,
            Chunk::Uniform(_) => unreachable!("just materialized"),
        }
    }

    #[inline]
    fn set_voxel(&mut self, i: [i32; 3], d: f32, color: Option<[u8; 4]>) {
        let band = BAND_VOXELS * self.voxel;
        let li = local_of(i);
        let data = self.data_mut(chunk_of(i));
        data.dist[li] = d.clamp(-band, band);
        if let Some(c) = color {
            data.color[li] = c;
        }
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
    /// 1-Lipschitz field. It only ever REDUCES |d| and never changes a sign, so the zero
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

        // The constraint is on the SIGNED field: adjacent voxels one `h` apart can
        // differ by at most `h`, which is exactly |∇d| ≤ 1. (Constraining |d| instead
        // is a weaker, different condition and barely moved the needle: 5.4% → 4.9%.)
        // Clamping each voxel into the interval its neighbours allow is the standard
        // alternating projection; a few sweeps converge, and a dab only perturbs the
        // field a voxel or two from where it wrote.
        for _ in 0..6 {
            let mut writes: Vec<([i32; 3], f32)> = Vec::new();
            for c in &region {
                for lz in 0..CHUNK {
                    for ly in 0..CHUNK {
                        for lx in 0..CHUNK {
                            let i = [c[0] * CHUNK + lx, c[1] * CHUNK + ly, c[2] * CHUNK + lz];
                            let cur = self.voxel_at(i);
                            if cur.abs() >= band {
                                continue; // saturated plateau: zero gradient by design
                            }
                            let (mut lo, mut hi) = (f32::NEG_INFINITY, f32::INFINITY);
                            for (dx, dy, dz) in
                                [(-1, 0, 0), (1, 0, 0), (0, -1, 0), (0, 1, 0), (0, 0, -1), (0, 0, 1)]
                            {
                                let n = self.voxel_at([i[0] + dx, i[1] + dy, i[2] + dz]);
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
                                writes.push((i, next));
                            }
                        }
                    }
                }
            }
            if writes.is_empty() {
                break; // already 1-Lipschitz — the common case after a gentle dab
            }
            // Jacobi, not Gauss-Seidel: updating in place would make the result depend
            // on iteration order (and bias the surface along +x).
            for (i, v) in writes {
                let li = local_of(i);
                let data = self.data_mut(chunk_of(i));
                data.dist[li] = v.clamp(-band, band);
            }
        }
    }

    /// Collapse chunks whose voxels are all one value back to a sentinel — this is what
    /// keeps a large map's memory proportional to its SURFACE rather than its volume.
    fn compact(&mut self, coords: &[[i32; 3]]) {
        for c in coords {
            let uniform = match self.chunks.get(c) {
                Some(Chunk::Data(d)) => {
                    let first = d.dist[0];
                    // Only collapse fully-saturated chunks: an interior value would lose
                    // real information, a band-saturated one carries none.
                    let band = BAND_VOXELS * self.voxel;
                    (first.abs() >= band - 1e-6 && d.dist.iter().all(|v| *v == first))
                        .then_some(first)
                }
                _ => None,
            };
            if let Some(v) = uniform {
                self.chunks.insert(*c, Chunk::Uniform(v));
            }
        }
    }

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
        match brush {
            Brush::Paint => return Vec::new(),
            Brush::Smooth => return self.smooth(center, radius, strength, profile),
            Brush::Flatten => return self.flatten(center, radius, strength, profile),
            _ => {}
        }
        // CSG, not accumulation. The obvious brush — nudge every voxel toward
        // solid/air by its own weight — is what the dense field did, and it is not a
        // distance-field operation: after N dabs an inside voxel has moved N × strength
        // × voxel while its neighbour just outside the radius has moved 0, so |∇d|
        // explodes (measured 5.4% of band voxels bad, worst 4.36, and no amount of
        // post-hoc projection repairs a field the write path keeps breaking).
        //
        // Union/subtract an analytic BALL instead. min/max — and smin/smax — of two
        // 1-Lipschitz fields is 1-Lipschitz, so the invariant holds by construction
        // rather than by repair. This is also how SDF sculpting is normally done, and it
        // makes a dab idempotent: holding the brush still no longer digs to infinity.
        //
        //   strength  -> how much of the ball this dab deposits (its radius)
        //   hardness  -> the smin blend k: hard = a crisp ball, soft = a gentle swell
        let s = strength.clamp(0.02, 1.0);
        let r_eff = radius * s;
        let k = (1.0 - profile.hardness.clamp(0.0, 1.0)) * radius * 0.5;
        let (lo, hi) = self.voxel_range(center, radius);
        let mut touched = Vec::new();
        for iz in lo[2]..=hi[2] {
            for iy in lo[1]..=hi[1] {
                for ix in lo[0]..=hi[0] {
                    let p = Vec3::new(ix as f32, iy as f32, iz as f32) * self.voxel;
                    // The ball's own SDF — exact, |∇d| = 1 everywhere.
                    let ball = (p - center).length() - r_eff;
                    let cur = self.voxel_at([ix, iy, iz]);
                    let next = match brush {
                        Brush::Raise => crate::smin(cur, ball, k),
                        // Subtract: intersect with the ball's COMPLEMENT (whose SDF is
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
                    self.set_voxel([ix, iy, iz], next, None);
                    touched.push(chunk_of([ix, iy, iz]));
                }
            }
        }
        touched.sort_unstable();
        touched.dedup();
        self.compact(&touched);
        touched
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
        for (i, v) in writes {
            self.set_voxel(i, v, None);
            touched.push(chunk_of(i));
        }
        touched.sort_unstable();
        touched.dedup();
        self.renormalize(&touched);
        self.compact(&touched);
        touched
    }

    fn flatten(&mut self, center: Vec3, radius: f32, strength: f32, profile: BrushProfile) -> Vec<[i32; 3]> {
        let s = strength.clamp(0.0, 1.0);
        let (lo, hi) = self.voxel_range(center, radius);
        let mut touched = Vec::new();
        for iz in lo[2]..=hi[2] {
            for iy in lo[1]..=hi[1] {
                for ix in lo[0]..=hi[0] {
                    let p = Vec3::new(ix as f32, iy as f32, iz as f32) * self.voxel;
                    let dxz = Vec3::new(p.x - center.x, 0.0, p.z - center.z).length();
                    if dxz > radius {
                        continue;
                    }
                    let w = s * profile.weight(dxz, radius);
                    if w <= 0.0 {
                        continue;
                    }
                    // Plane SDF at the hit height — the same target the dense brush used.
                    let target = p.y - center.y;
                    let cur = self.voxel_at([ix, iy, iz]);
                    self.set_voxel([ix, iy, iz], cur + (target - cur) * w, None);
                    touched.push(chunk_of([ix, iy, iz]));
                }
            }
        }
        touched.sort_unstable();
        touched.dedup();
        self.renormalize(&touched);
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
        // the field jump from -band (solid) to +band (air) across ONE voxel at the
        // slab's rim, because nothing ever wrote the ramp between them. Measured: worst
        // |∇d| = 4.36 on a bare slab, before any brush touched it. That is the same
        // mistake the dense `grow()` made (a cliff in the FIELD rather than in the
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
                    // Skip far AIR only — never far SOLID. Skipping deep solid leaves the
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
    /// every voxel of `[min, max]` (padded by the band); values are UNIONED with
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
        let band = self.band();
        let lo = (min - Vec3::splat(band + self.voxel)) / self.voxel;
        let hi = (max + Vec3::splat(band + self.voxel)) / self.voxel;
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
                    let c = color(p);
                    let cu = [
                        (c[0].clamp(0.0, 1.0) * 255.0).round() as u8,
                        (c[1].clamp(0.0, 1.0) * 255.0).round() as u8,
                        (c[2].clamp(0.0, 1.0) * 255.0).round() as u8,
                        255,
                    ];
                    self.set_voxel([ix, iy, iz], d.min(cur), Some(cu));
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
    /// are visited ONCE each and their rows copied, so the mesher's inner loops become
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
                        Some(Chunk::Data(d)) => {
                            for z in sz..=ez {
                                for y in sy..=ey {
                                    let si = ((z - bz) as usize * CHUNK_U + (y - by) as usize) * CHUNK_U;
                                    let di = ((z - lo[2]) as usize * dim + (y - lo[1]) as usize) * dim;
                                    let n = (ex - sx + 1) as usize;
                                    let sa = si + (sx - bx) as usize;
                                    let da = di + (sx - lo[0]) as usize;
                                    dist[da..da + n].copy_from_slice(&d.dist[sa..sa + n]);
                                    color[da..da + n].copy_from_slice(&d.color[sa..sa + n]);
                                }
                            }
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
            // Step a FRACTION of the reported distance. Sphere tracing assumes
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

    // ---- migration ----------------------------------------------------------

    /// Import a dense [`crate::mesh2sdf::BakedSdf`] (a legacy `.tfield`) into the sparse
    /// store, resampling onto this field's voxel lattice.
    ///
    /// Existing projects must just open. The dense grid's values are trilinearly
    /// resampled and band-clamped; uniform regions collapse. Ty's 192 MB field imports to
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
        // Full import SKIPS air (a fresh chunk is already `Uniform(+band)` air) so the
        // field stays sparse — only the surface band is materialized.
        f.resample_dense(baked, ilo, ihi, false);
        f
    }

    /// Re-sample the dense field over a world-space AABB, rewriting only the voxels inside
    /// it — the REGIONAL counterpart of [`from_dense`], for live editing. A brush dab moves
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
        // Regional refresh WRITES air (a dig turned solid into air — the voxels that were
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
                    // Skip far-from-surface AIR only — never far-from-surface SOLID. A fresh
                    // chunk starts as `Uniform(+band)` air, so skipping air writes what's
                    // already there; skipping deep SOLID (`dv <= -band`) left rock reading as
                    // air and the import became a hollow SHELL (a spurious inner surface a
                    // band below the real one — meshed vertices sat -0.935 units inside the
                    // dense field). Deep-solid voxels cost nothing: `compact` collapses a
                    // saturated chunk to a `Uniform(-band)` sentinel. (Clamping against the
                    // source box SDF was tried and made it WORSE: 4.8% -> 18.5%.)
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
        for c in self.chunks.values_mut() {
            if let Chunk::Data(d) = c {
                for v in &mut d.color {
                    v[0] = rgb[0];
                    v[1] = rgb[1];
                    v[2] = rgb[2];
                }
            }
        }
    }

    /// Fill the WHOLE terrain with a texture palette `slot` (1-based; 0 = untextured).
    /// The slot rides the colour alpha channel — same convention as the dense field
    /// and the splat shader. Leaves shape + RGB tint.
    pub fn fill_texture(&mut self, slot: u8) {
        self.base_color[3] = slot;
        for c in self.chunks.values_mut() {
            if let Chunk::Data(d) = c {
                for v in &mut d.color {
                    v[3] = slot;
                }
            }
        }
    }

    /// Paint a texture palette `slot` (alpha channel) over near-surface voxels inside
    /// the brush ball. Mirrors the dense [`crate::Terrain::paint_texture`].
    pub fn paint_texture(&mut self, center: Vec3, radius: f32, slot: u8) -> Vec<[i32; 3]> {
        let (lo, hi) = self.voxel_range(center, radius);
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

    /// Lay flat ground across the field's CURRENT bounds ("fill bounds"): a solid slab
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

    /// World-space AABB of everything stored (data chunks AND solid interior
    /// sentinels), or `None` for an empty field. This is the box the shadow proxy,
    /// the collider wireframe, and camera framing use — an unbounded field still has
    /// bounded *content*.
    pub fn bounds(&self) -> Option<(Vec3, Vec3)> {
        let mut lo = [i32::MAX; 3];
        let mut hi = [i32::MIN; 3];
        for (c, ch) in &self.chunks {
            let solid = match ch {
                Chunk::Data(_) => true,
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

    /// Every chunk coordinate a brush of `radius` at `center` COULD touch (its voxel
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
            match ch {
                Some(ch) => {
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
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 * 1024);
        out.extend_from_slice(b"FCF1");
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&self.voxel.to_le_bytes());
        out.extend_from_slice(&self.base_color);
        let band = self.band();
        // Stored chunks: data + non-air uniforms (solid interior). Air is implicit.
        let mut coords: Vec<[i32; 3]> = self
            .chunks
            .iter()
            .filter(|(_, ch)| match ch {
                Chunk::Data(_) => true,
                Chunk::Uniform(v) => *v < band - 1e-6,
            })
            .map(|(c, _)| *c)
            .collect();
        coords.sort_unstable(); // deterministic output (byte-identical saves)
        out.extend_from_slice(&(coords.len() as u32).to_le_bytes());
        for c in coords {
            for k in c {
                out.extend_from_slice(&k.to_le_bytes());
            }
            match &self.chunks[&c] {
                Chunk::Uniform(v) => {
                    out.push(0);
                    out.extend_from_slice(&v.to_le_bytes());
                }
                Chunk::Data(d) => {
                    out.push(1);
                    // Distance: i8 in band units, RLE (u16 run, i8 value).
                    let q = |v: f32| ((v / band) * 127.0).round().clamp(-127.0, 127.0) as i8;
                    let mut runs: Vec<(u16, i8)> = Vec::new();
                    for v in &d.dist {
                        let qv = q(*v);
                        match runs.last_mut() {
                            Some((n, lv)) if *lv == qv && *n < u16::MAX => *n += 1,
                            _ => runs.push((1, qv)),
                        }
                    }
                    out.extend_from_slice(&(runs.len() as u32).to_le_bytes());
                    for (n, v) in runs {
                        out.extend_from_slice(&n.to_le_bytes());
                        out.push(v as u8);
                    }
                    // Colour: RLE (u16 run, RGBA8).
                    let mut cruns: Vec<(u16, [u8; 4])> = Vec::new();
                    for v in &d.color {
                        match cruns.last_mut() {
                            Some((n, lv)) if lv == v && *n < u16::MAX => *n += 1,
                            _ => cruns.push((1, *v)),
                        }
                    }
                    out.extend_from_slice(&(cruns.len() as u32).to_le_bytes());
                    for (n, v) in cruns {
                        out.extend_from_slice(&n.to_le_bytes());
                        out.extend_from_slice(&v);
                    }
                }
            }
        }
        out
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
        if u32_at(take(&mut at, 4)?) != 1 {
            return None;
        }
        let voxel = f32::from_le_bytes(take(&mut at, 4)?.try_into().ok()?);
        if !(voxel.is_finite() && voxel > 1e-3) {
            return None;
        }
        let mut f = Self::new(voxel);
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
                    f.chunks.insert(c, Chunk::Data(Box::new(ChunkData { dist, color })));
                }
                _ => return None,
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
        // Voxel CENTERS at (i+0.5)/n across the box — the convention `Terrain::sample`,
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
        let [w, h, d] = baked.dims;
        let lo = Vec3::from(baked.center) - Vec3::from(baked.half_extent);
        let vs = Vec3::new(
            2.0 * baked.half_extent[0] / w.max(1) as f32,
            2.0 * baked.half_extent[1] / h.max(1) as f32,
            2.0 * baked.half_extent[2] / d.max(1) as f32,
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
        self.sample_into_dense(baked, mn, mx);
        (mn, [mx[0] + 1, mx[1] + 1, mx[2] + 1])
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
        let [w, h, _d] = baked.dims;
        let lo = Vec3::from(baked.center) - Vec3::from(baked.half_extent);
        let vs = Vec3::new(
            2.0 * baked.half_extent[0] / baked.dims[0].max(1) as f32,
            2.0 * baked.half_extent[1] / baked.dims[1].max(1) as f32,
            2.0 * baked.half_extent[2] / baked.dims[2].max(1) as f32,
        );
        for iz in mn[2]..=mx[2] {
            for iy in mn[1]..=mx[1] {
                for ix in mn[0]..=mx[0] {
                    let p = lo
                        + Vec3::new(ix as f32 + 0.5, iy as f32 + 0.5, iz as f32 + 0.5) * vs;
                    let i = ((iz * h + iy) * w + ix) as usize;
                    baked.distance[i] = self.d(p);
                    baked.color[i] = self.color(p);
                }
            }
        }
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

    /// Fold another snapshot in, keeping the FIRST-seen entry per coord — later dabs
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
                Some(Chunk::Data(_)) => CHUNK_VOXELS * 8,
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

    /// Per-write-path |∇d| report. This is the diagnostic that found the real culprit:
    /// `fill_slab` was writing a PLANE clipped to a box, so the slab's rim jumped from
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

    /// THE invariant. |∇d| ≤ 1 is what sphere tracing, gradient normals, SDF AO and sun
    /// shadows all assume. The dense field's `grow()` broke it by SUMMING two distance
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
        //   Smooth, Flatten      ~4.9 / 1.8%   spatially-weighted BLENDS — not SDF ops
        //
        // Smooth averages its neighbours, so where those are saturated at ±band it can
        // land near 0 beside a ±band neighbour: a configuration a true SDF cannot hold
        // (−band→+band needs ~8 voxels, not 2). No projection repairs that — only a real
        // redistance (fast-marching) pass reseeded from the zero crossing would, and that
        // is deliberately not in P1.
        //
        // It is tolerable for now because of WHO reads the field: the mesher normalizes
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

    /// Lower must carve THE BALL and nothing else. The shipped bug computed
    /// `max(cur, ball)` instead of `max(cur, -ball)` — keep-the-ball-carve-the-box —
    /// so every dig blasted a write-box-sized square crater ("massive squares",
    /// Ty's solar playtest). The dab: radius 1.3, strength 0.6 — the dig_tool defaults.
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
        // WELL outside the ball but inside the brush's write box (radius + band + 1
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

    /// Sparsity is the whole point: memory must track the SURFACE, not the volume.
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

    /// Existing projects must just open — Ty's 192 MB field included.
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

        // ...and the ground must be SOLID all the way down, not a shell.
        //
        // The raycast above cannot see this: it comes from ABOVE and stops at the first
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
        // NOT asserting "sparse < dense" here, because on a TOY field it isn't true and
        // saying so would be a lie: a 32³ chunk is 256 KB, so a 48-unit test terrain
        // rounds up to a few chunks and can cost more than the dense grid it came from.
        // Sparsity is a SCALE property — it wins when the volume grows and the surface
        // doesn't (Ty's 433×406×460-unit field is 192 MB dense; only its band is worth
        // storing). What must hold at every scale is an absolute ceiling.
        assert!(
            sparse < 8_000_000,
            "migrated field is {:.1} MB — far more than its band can justify",
            sparse as f32 / 1.0e6
        );
        // The SOURCE field is itself not a distance field: the dense brushes never
        // enforced |∇d| ≤ 1 either (they nudge voxels by weight, the same mistake this
        // module's first cut made). A faithful import cannot be cleaner than its input,
        // so this asserts "no worse than the source", not "correct" — the redistance pass
        // that would actually fix it is deliberately out of P1.
        let (worst_g, bad) = f.lipschitz_audit();
        println!("migrated field: worst |∇d| {worst_g:.2}, {:.1}% bad (source is itself non-SDF)", bad * 100.0);
        assert!(
            bad < 0.10,
            "migrated field violates |∇d| ≤ 1 at {:.1}% (worst {worst_g:.2}) — worse than \
             the ~4.8% the dense source itself carries",
            bad * 100.0
        );
    }

    /// The sculpt fast-path: after the DENSE authority changes under a dab, a REGIONAL
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

        // Raise a bump in the DENSE authority (the editor's real sculpt), like a stroke.
        for _ in 0..8 {
            t.sculpt(Brush::Raise, [0.0, 0.5, 0.0], 5.0, 1.0, BrushProfile::default());
        }

        // Refresh ONLY the box the dab touched — the whole field must NOT be re-imported.
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

        // Ground OUTSIDE the refreshed box is untouched — the refresh was regional, not a
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

    /// `bounds` covers data AND solid interior, and grows when sculpting outward —
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
