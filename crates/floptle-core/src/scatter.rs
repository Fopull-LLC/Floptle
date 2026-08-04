//! Scatter: thousands of props from a seed instead of thousands of nodes
//! (`floptle/0036`).
//!
//! A game that wants a forest has, until now, had exactly one construction API:
//! `createNode` + `setPrimitive` + `setMaterial`. A plant is 4–14 nodes, so a
//! "forest" is a moving bubble of ninety plants, none of which you can walk
//! into, because a script cannot give a node it created a collider. Both of
//! those are engine problems wearing a game's clothes.
//!
//! ## The shape of the answer
//!
//! The game keeps deciding **what grows where** — species, climate, palette,
//! yields — and hands the engine a prototype and a rule. The engine decides
//! **where each instance is** and draws them all. That split is deliberate: the
//! alternative (the engine growing its own plant generator) is both worse and
//! less general, and the card says so.
//!
//! ## Determinism is the whole design
//!
//! Every instance is derived from `hash(seed, chunk, index)` and nothing else —
//! no accumulated state, no iteration order, no floating-point history. Three
//! things fall out of that, and all three are requirements rather than
//! conveniences:
//!
//! * **Walk away and back and the same trees stand in the same places.** A
//!   chunk is recomputed, not remembered.
//! * **A multiplayer session never replicates scenery.** Same seed, same
//!   chunk, same instances, on every machine.
//! * **A removal set is small.** "Every plant you ever cut" is unstorable; the
//!   ids of the ones you cut are a handful of `u64`s, and an id is stable
//!   because the placement that produced it is.

use std::collections::HashSet;

use crate::math::{DVec3, Quat, Vec3};

/// A stable per-instance identifier: `hash(source, chunk, index)`.
///
/// Stable across a stream-out and back in, which is what makes "this tree is
/// gone" storable at all — the alternative is remembering positions, and
/// positions are floats that came from a chain of arithmetic.
pub type InstanceId = u64;

/// Where a scatter source places things.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Region {
    /// The surface of a sphere — a planet. Instances are placed on a cube-face
    /// grid projected onto the sphere, which is what keeps their density even
    /// instead of piling them at the poles.
    Sphere { center: DVec3, radius: f64 },
    /// A flat rectangle in the XZ plane at `y` — a level, an island, a lawn.
    Ground { center: DVec3, half: [f64; 2] },
}

/// How an instance is oriented.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    /// Straight up in world terms — right for a level with a flat floor.
    World,
    /// Along the surface normal — right for a planet, where "up" is different
    /// at every point, and for a hillside, where a vertical tree looks wrong.
    #[default]
    Surface,
}

/// One drawable form of a prop, and the distance it is used out to.
#[derive(Clone, Debug, PartialEq)]
pub struct Band {
    /// The mesh asset drawn in this band.
    pub asset: String,
    /// Used from the camera out to this distance (metres).
    pub distance: f32,
}

// There is no collision proxy here, and the absence is deliberate. One used to
// be parsed, defaulted, stored — and read by nothing at all, so a game that
// asked for solid props got no error and props it walked through
// (`floptle/0066`). Scattered instances are drawn, queried (`scatter.near`) and
// removed; they are not in the physics world. When that changes it will be a
// feature with a test, not a field.

/// A per-position density in 0..1, sampled instead of called.
///
/// **Why a grid and not a callback.** Placement has to stay a pure function of
/// `(seed, chunk, index)`: walk away and back and the same props must stand in
/// the same places, and two machines must agree without replicating anything. A
/// hook that ran per instance could read live game state and answer differently
/// on the second visit, which breaks that outright — and it would put a script
/// call inside chunk generation besides.
///
/// So a game's rule is evaluated ONCE, when the source is declared, and what
/// the engine keeps is the answer. Cheap to sample, deterministic by
/// construction, and it replicates as a small array if it ever needs to.
#[derive(Clone, Debug, PartialEq)]
pub struct Density {
    /// Rows. A sphere is sampled equirectangularly, so it has `2 × rows`
    /// columns; a ground region is square, `rows × rows`.
    pub rows: u32,
    /// Row-major, `0..1`. Row 0 is the north pole / the region's -Z edge.
    pub data: Vec<f32>,
}

impl Density {
    /// Columns: twice the rows for a sphere's equirectangular map, else square.
    fn cols(&self, sphere: bool) -> u32 {
        if sphere { self.rows * 2 } else { self.rows }
    }

    /// Bilinear sample at `(u, v)`, both `0..1`. Out of range clamps — the edge
    /// of the map is the edge of the world it describes.
    fn sample(&self, u: f64, v: f64, sphere: bool) -> f32 {
        let (rows, cols) = (self.rows.max(1), self.cols(sphere).max(1));
        if self.data.len() < (rows * cols) as usize {
            return 1.0;
        }
        let at = |r: u32, c: u32| self.data[(r.min(rows - 1) * cols + c.min(cols - 1)) as usize];
        let (fu, fv) = (u.clamp(0.0, 1.0) * (cols - 1) as f64, v.clamp(0.0, 1.0) * (rows - 1) as f64);
        let (c0, r0) = (fu.floor() as u32, fv.floor() as u32);
        let (tu, tv) = ((fu - c0 as f64) as f32, (fv - r0 as f64) as f32);
        let (a, b) = (at(r0, c0), at(r0, c0 + 1));
        let (c, d) = (at(r0 + 1, c0), at(r0 + 1, c0 + 1));
        let top = a + (b - a) * tu;
        let bot = c + (d - c) * tu;
        top + (bot - top) * tv
    }
}

/// A scatter source: one rule, many instances.
#[derive(Clone, Debug)]
pub struct ScatterSource {
    pub id: u32,
    pub seed: u64,
    pub region: Region,
    /// Instances per chunk. The chunk is the unit of determinism and of
    /// streaming, so density is expressed against it rather than per m² —
    /// a per-m² figure would silently change the instance COUNT (and therefore
    /// every id) whenever the chunk size changed.
    pub per_chunk: u32,
    /// Chunk edge in world units.
    pub chunk: f64,
    pub align: Align,
    /// Uniform scale range, `(min, max)`.
    pub scale: (f32, f32),
    /// LOD bands, nearest first. The last band's distance is the cull range.
    pub bands: Vec<Band>,
    /// Metres of cross-fade at each band boundary, so nothing pops.
    pub fade: f32,
    /// Where this source grows and how thickly, `0..1` (`None` = everywhere,
    /// evenly). A density of 0 produces NO instance — not a hidden one, or the
    /// whole point of scattering is lost.
    pub density: Option<Density>,
    /// Instances the game has removed (harvested, dug out from under).
    pub removed: HashSet<InstanceId>,
}

impl ScatterSource {
    /// The cull distance — the last band's.
    ///
    /// **This is the budget, not a look.** It sets how many chunks are resident,
    /// as a sweep whose side grows with it, and that sweep is walked every
    /// frame. See [`cost`].
    pub fn range(&self) -> f32 {
        self.bands.last().map(|b| b.distance).unwrap_or(0.0)
    }
}

/// What a source's configuration costs every frame, countable before a game
/// ships it (`floptle/0071`).
///
/// The knobs read as a look — how far props are drawn, how big a chunk is, how
/// many per chunk — and one of them is secretly the whole budget. A field
/// configured at `far = 700, chunk = 22` on a 214-unit planet came to 4,489
/// chunks and ~117,000 props **per source**, and nothing in the API, the docs or
/// the Console said so until the game was unplayable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cost {
    /// Chunks resident at once.
    pub chunks: u64,
    /// Props in them, at full density.
    pub props: u64,
}

/// Count what [`Cost`] describes, by running the real sweep.
///
/// Deliberately not a formula. `chunks_near` is the definition of residency, so
/// counting its answer is the only version of this that cannot drift from the
/// thing it claims to measure — and at a few thousand keys, once, at declare
/// time, the honesty is free.
pub fn cost(src: &ScatterSource) -> Cost {
    if src.range() <= 0.0 || src.bands.is_empty() {
        return Cost::default();
    }
    // A representative eye: standing in the middle of a ground region, or on
    // the surface of a planet. Residency is the same size wherever you stand.
    let eye = match src.region {
        Region::Ground { center, .. } => center,
        Region::Sphere { center, radius } => center + DVec3::new(0.0, radius, 0.0),
    };
    let chunks = chunks_near(src, eye, src.range() as f64).len() as u64;
    Cost { chunks, props: chunks * src.per_chunk as u64 }
}

/// One resolved instance, ready to draw or to collide with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Instance {
    pub id: InstanceId,
    /// World position, BEFORE the caller drops it onto the real surface.
    pub pos: DVec3,
    /// Surface normal used for `Align::Surface` (world +Y for `Align::World`).
    pub up: Vec3,
    /// Rotation about `up`.
    pub yaw: f32,
    pub scale: f32,
    /// A free per-instance 0..1 the game can map to a colour, a variant, a
    /// yield — rolled from the same hash, so it is as stable as the position.
    pub param: f32,
}

impl Instance {
    /// The instance's orientation, given its alignment mode.
    pub fn rotation(&self, align: Align) -> Quat {
        let spin = Quat::from_rotation_y(self.yaw);
        match align {
            Align::World => spin,
            // Tilt +Y onto the surface normal, THEN spin about it — the other
            // order spins about world +Y and leaves a hillside's trees all
            // facing the same way as they lean.
            Align::Surface => Quat::from_rotation_arc(Vec3::Y, self.up) * spin,
        }
    }
}

/// A 64-bit mix (SplitMix64's finalizer). Deterministic, cheap, and — unlike a
/// `DefaultHasher` — guaranteed to give the same answer in every build of every
/// binary, which is the entire requirement.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut x = z;
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// `mix` over several words, so a (seed, chunk, index) triple is one id.
fn hash3(a: u64, b: u64, c: u64) -> u64 {
    mix(mix(mix(a) ^ b) ^ c)
}

/// A 0..1 float from a hash word. Uses the HIGH bits: the low bits of a
/// multiply-xorshift mix are the weakest, and a lattice in the low bits shows up
/// as trees in rows.
fn unit(h: u64) -> f64 {
    (h >> 11) as f64 / (1u64 << 53) as f64
}

/// A chunk's integer key. Ground regions use `(x, z)`; sphere regions use
/// `(face, u, v)` packed into the same two words, so one type covers both.
pub type ChunkKey = (i64, i64);

/// Every instance in one chunk, derived from nothing but `(seed, key, index)`.
///
/// Removed instances are skipped here rather than filtered by the caller, so
/// "gone" costs nothing downstream — no draw, no collider, no id to test again.
pub fn chunk_instances(src: &ScatterSource, key: ChunkKey) -> Vec<Instance> {
    let mut out = Vec::with_capacity(src.per_chunk as usize);
    let ck = mix((key.0 as u64) << 32 ^ (key.1 as u64 & 0xFFFF_FFFF));
    for i in 0..src.per_chunk {
        let id = hash3(src.seed, ck, i as u64);
        if src.removed.contains(&id) {
            continue;
        }
        // Four independent streams off the same id — deriving them by shifting
        // ONE value correlates position with scale, and a forest whose big
        // trees are all in the north-east reads as a bug you cannot name.
        let (hx, hz, hy, hp) =
            (mix(id), mix(id ^ 0xA1), mix(id ^ 0xB2), mix(id ^ 0xC3));
        let (fx, fz) = (unit(hx), unit(hz));
        let (pos, up) = match src.region {
            Region::Ground { center, half } => {
                let x = (key.0 as f64 + fx) * src.chunk;
                let z = (key.1 as f64 + fz) * src.chunk;
                let p = DVec3::new(center.x + x, center.y, center.z + z);
                // Outside the declared rectangle → this roll produced nothing.
                // Rejecting rather than clamping keeps the density even right
                // up to the edge instead of building a wall of props on it.
                if (p.x - center.x).abs() > half[0] || (p.z - center.z).abs() > half[1] {
                    continue;
                }
                (p, Vec3::Y)
            }
            Region::Sphere { center, radius } => {
                // Cube-face projection: the chunk grid lives on a cube, and
                // each point is normalised onto the sphere. Even coverage with
                // no pole pile-up, and it is the same mapping the terrain
                // streamer uses so chunks line up with the ground they sit on.
                let face = key.0.rem_euclid(6) as usize;
                let gu = (key.0 >> 3) as f64;
                let gv = key.1 as f64;
                let (u, v) = (
                    ((gu + fx) * src.chunk / radius).clamp(-1.0, 1.0),
                    ((gv + fz) * src.chunk / radius).clamp(-1.0, 1.0),
                );
                let dir = cube_face_dir(face, u, v).normalize();
                (center + dir * radius, dir.as_vec3())
            }
        };
        // Density: a fresh hash stream, so surviving is uncorrelated with where
        // you stand and how big you are. Rejecting rather than thinning at draw
        // time is what makes an empty biome cost nothing.
        if let Some(d) = &src.density {
            let (u, v) = match src.region {
                Region::Ground { center, half } => (
                    ((pos.x - center.x) / (2.0 * half[0]) + 0.5),
                    ((pos.z - center.z) / (2.0 * half[1]) + 0.5),
                ),
                Region::Sphere { center, .. } => {
                    // Equirectangular: v is latitude from the north pole, u is
                    // longitude — the same map a climate model is written on.
                    let dir = (pos - center).normalize_or_zero();
                    (
                        dir.z.atan2(dir.x) / std::f64::consts::TAU + 0.5,
                        dir.y.clamp(-1.0, 1.0).acos() / std::f64::consts::PI,
                    )
                }
            };
            let sphere = matches!(src.region, Region::Sphere { .. });
            if unit(mix(id ^ 0xE5)) as f32 >= d.sample(u, v, sphere) {
                continue;
            }
        }
        let (smin, smax) = src.scale;
        out.push(Instance {
            id,
            pos,
            up,
            yaw: (unit(hy) * std::f64::consts::TAU) as f32,
            scale: smin + (smax - smin) * unit(hp) as f32,
            param: unit(mix(id ^ 0xD4)) as f32,
        });
    }
    out
}

/// The outward direction for a point `(u, v)` in `[-1, 1]²` on cube face `f`.
fn cube_face_dir(f: usize, u: f64, v: f64) -> DVec3 {
    match f {
        0 => DVec3::new(1.0, v, -u),
        1 => DVec3::new(-1.0, v, u),
        2 => DVec3::new(u, 1.0, -v),
        3 => DVec3::new(u, -1.0, v),
        4 => DVec3::new(u, v, 1.0),
        _ => DVec3::new(-u, v, -1.0),
    }
}

/// Which LOD band a distance falls in, and how far it is faded into the NEXT
/// one (`0` = fully this band, `1` = fully the next).
///
/// Returned as a blend rather than a hard index because the pop at a band
/// boundary is the thing everyone notices about scatter and nothing else about
/// it. Past the last band the instance is gone: `None`.
pub fn band_at(src: &ScatterSource, dist: f32) -> Option<(usize, f32)> {
    let fade = src.fade.max(0.0);
    for (i, b) in src.bands.iter().enumerate() {
        if dist < b.distance {
            // Inside a fade window BEFORE this band's far edge, and there is
            // something to fade into.
            let into = b.distance - dist;
            if into < fade && i + 1 < src.bands.len() {
                return Some((i, 1.0 - into / fade.max(1e-4)));
            }
            return Some((i, 0.0));
        }
    }
    // Past the last band — but fade OUT across the final window rather than
    // vanishing, so the horizon dissolves instead of snapping.
    None
}

/// The world-space centre of a chunk.
///
/// Approximate on a sphere — a cube-face cell projects to a patch, not a disc,
/// and its centre is off by a fraction of a chunk near a face edge. Everything
/// that reads this allows a whole chunk of slack, so the approximation costs
/// nothing.
pub fn chunk_center(src: &ScatterSource, key: ChunkKey) -> DVec3 {
    match src.region {
        Region::Ground { center, .. } => DVec3::new(
            center.x + (key.0 as f64 + 0.5) * src.chunk,
            center.y,
            center.z + (key.1 as f64 + 0.5) * src.chunk,
        ),
        Region::Sphere { center, radius } => {
            let face = key.0.rem_euclid(6) as usize;
            let (gu, gv) = ((key.0 >> 3) as f64, key.1 as f64);
            let u = ((gu + 0.5) * src.chunk / radius).clamp(-1.0, 1.0);
            let v = ((gv + 0.5) * src.chunk / radius).clamp(-1.0, 1.0);
            center + cube_face_dir(face, u, v).normalize_or_zero() * radius
        }
    }
}

/// Which chunk the eye is standing in. The key set changes when this changes —
/// which is when the sweep is worth redoing, and not once a frame
/// (`floptle/0071`).
pub fn eye_chunk(src: &ScatterSource, eye: DVec3) -> ChunkKey {
    match src.region {
        Region::Ground { center, .. } => {
            let local = eye - center;
            ((local.x / src.chunk).floor() as i64, (local.z / src.chunk).floor() as i64)
        }
        Region::Sphere { center, radius } => {
            let dir = (eye - center).normalize_or_zero();
            let face = dominant_face(dir);
            let (u, v) = face_uv(face, dir);
            (
                (((u * radius / src.chunk).floor() as i64) << 3) | face as i64,
                (v * radius / src.chunk).floor() as i64,
            )
        }
    }
}

/// Every chunk key whose chunk could contain something within `range` of `eye`,
/// **nearest first**.
///
/// Deliberately generous: a chunk is included if its CENTRE is within
/// `range + chunk`, so a prop near a chunk's far corner is never culled by the
/// chunk it happens to live in. Missing props at a chunk seam is the classic
/// scatter bug and it only shows up as you walk.
///
/// The order is load-bearing, not tidiness. A draw budget cuts the tail of this
/// list, and the tail has to be the far side of the world rather than whichever
/// chunks a nested loop happened to reach last — a budget that drops props at
/// your feet and keeps the ones at the horizon is worse than no budget. The
/// same order is what makes streaming spend its first frames on what you can
/// actually see.
///
/// The square sweep is also cut to a disc here: a corner of the swept square is
/// √2 range away and can hold nothing visible, and on the bad configuration
/// that is a fifth of the chunks walked every frame for nothing.
///
/// **The answer depends on the eye's CHUNK, not on the eye.** Both the cull and
/// the order measure from the centre of the chunk the eye stands in, so walking
/// across a chunk cannot change the list — which is what makes it cacheable
/// until you cross a boundary, and the slack below is sized for it.
pub fn chunks_near(src: &ScatterSource, eye: DVec3, range: f64) -> Vec<ChunkKey> {
    let mut keys: Vec<(f64, ChunkKey)> = Vec::new();
    let reach = range + src.chunk;
    // Two chunks of slack, measured centre to centre: one for the eye sitting
    // in the corner of its own chunk, one for a prop sitting in the corner of
    // the chunk being measured. Each is worth at most 0.71 of a chunk, so this
    // is generous by design — a prop missing at a seam is the classic scatter
    // bug and it only shows up as you walk.
    let cull = (reach + src.chunk) * (reach + src.chunk);
    let from = chunk_center(src, eye_chunk(src, eye));
    let consider = |key: ChunkKey, keys: &mut Vec<(f64, ChunkKey)>| {
        let d = (chunk_center(src, key) - from).length_squared();
        if d <= cull {
            keys.push((d, key));
        }
    };
    match src.region {
        Region::Ground { center, half } => {
            let local = eye - center;
            let n = (reach / src.chunk).ceil() as i64;
            let (cx, cz) =
                ((local.x / src.chunk).floor() as i64, (local.z / src.chunk).floor() as i64);
            let (lx, lz) =
                ((half[0] / src.chunk).ceil() as i64, (half[1] / src.chunk).ceil() as i64);
            for x in (cx - n)..=(cx + n) {
                for z in (cz - n)..=(cz + n) {
                    if x.abs() <= lx + 1 && z.abs() <= lz + 1 {
                        consider((x, z), &mut keys);
                    }
                }
            }
        }
        Region::Sphere { center, radius } => {
            // Only the face the eye is over, plus a ring around it — the far
            // side of a planet is never within any sane view distance, and
            // walking all six faces would cost 6× for nothing.
            let dir = (eye - center).normalize_or_zero();
            let face = dominant_face(dir);
            let n = (reach / src.chunk).ceil() as i64;
            let (u, v) = face_uv(face, dir);
            let (cu, cv) = (
                (u * radius / src.chunk).floor() as i64,
                (v * radius / src.chunk).floor() as i64,
            );
            // …and only as far as the face actually goes. A cube face runs
            // `u, v` in [-1, 1], which is `radius / chunk` cells; a key beyond
            // that clamps to the edge and re-derives points already covered.
            // On a body SMALLER than the view distance that is nearly all of
            // the sweep: 700 m of `lod` on a 107 m planet swept 4,489 keys for
            // 174 distinct chunks, and piled three quarters of its props on the
            // seam in a wall (`floptle/0071`). Residency saturates at the body.
            //
            // The ground path has always clamped to its region this way; the
            // sphere path never did, and the difference was invisible until a
            // planet was small.
            let lim = (radius / src.chunk).ceil() as i64 + 1;
            for du in (cu - n).max(-lim)..=(cu + n).min(lim) {
                for dv in (cv - n).max(-lim)..=(cv + n).min(lim) {
                    consider(((du << 3) | face as i64, dv), &mut keys);
                }
            }
        }
    }
    keys.sort_by(|a, b| a.0.total_cmp(&b.0));
    keys.into_iter().map(|(_, k)| k).collect()
}

fn dominant_face(d: DVec3) -> usize {
    let a = d.abs();
    if a.x >= a.y && a.x >= a.z {
        if d.x > 0.0 { 0 } else { 1 }
    } else if a.y >= a.z {
        if d.y > 0.0 { 2 } else { 3 }
    } else if d.z > 0.0 {
        4
    } else {
        5
    }
}

fn face_uv(f: usize, d: DVec3) -> (f64, f64) {
    let a = d.abs();
    match f {
        0 => (-d.z / a.x, d.y / a.x),
        1 => (d.z / a.x, d.y / a.x),
        2 => (d.x / a.y, -d.z / a.y),
        3 => (d.x / a.y, d.z / a.y),
        4 => (d.x / a.z, d.y / a.z),
        _ => (-d.x / a.z, d.y / a.z),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ground(per_chunk: u32) -> ScatterSource {
        ScatterSource {
            id: 1,
            seed: 0xFEED,
            region: Region::Ground { center: DVec3::ZERO, half: [200.0, 200.0] },
            per_chunk,
            chunk: 16.0,
            align: Align::Surface,
            scale: (0.8, 1.4),
            bands: vec![
                Band { asset: "tree.glb".into(), distance: 40.0 },
                Band { asset: "tree_far.glb".into(), distance: 120.0 },
            ],
            fade: 8.0,
            density: None,
            removed: HashSet::new(),
        }
    }

    /// The load-bearing property: a chunk is RECOMPUTED, never remembered.
    /// Walk away and back and the same trees must stand in the same places —
    /// and on every machine, which is what lets a multiplayer session skip
    /// replicating scenery entirely.
    #[test]
    fn a_chunk_resolves_identically_every_time() {
        let s = ground(24);
        let a = chunk_instances(&s, (3, -7));
        let b = chunk_instances(&s, (3, -7));
        assert_eq!(a, b, "the same chunk gave two different answers");
        assert!(!a.is_empty());
    }

    /// Density: where the map says nothing grows, nothing is GENERATED — not
    /// hidden at draw time, or the reason to scatter at all is gone
    /// (`floptle/0064`).
    #[test]
    fn density_zero_grows_nothing_and_density_one_is_untouched() {
        let mut s = ground(64);
        let full = chunk_instances(&s, (0, 0));
        assert!(!full.is_empty());

        s.density = Some(Density { rows: 4, data: vec![0.0; 16] });
        assert!(chunk_instances(&s, (0, 0)).is_empty(), "a dead biome makes nothing");

        s.density = Some(Density { rows: 4, data: vec![1.0; 16] });
        assert_eq!(chunk_instances(&s, (0, 0)), full, "a full map changes nothing at all");
    }

    /// A map that is dense on one side and empty on the other puts the props on
    /// the dense side — the biome case, which is the whole point.
    #[test]
    fn a_half_empty_map_grows_on_one_side_only() {
        let mut s = ground(256);
        // 2×2: left column empty, right column full. u runs -X → +X.
        s.density = Some(Density { rows: 2, data: vec![0.0, 1.0, 0.0, 1.0] });
        let out = chunk_instances(&s, (0, 0));
        assert!(!out.is_empty(), "the dense half still grows");
        assert!(
            out.iter().all(|i| i.pos.x > -1e-9),
            "nothing grew in the empty half: {:?}",
            out.iter().map(|i| i.pos.x).collect::<Vec<_>>()
        );
    }

    /// The load-bearing property survives a mask: same chunk, same answer, and
    /// the ids of the survivors do not shift when their neighbours vanish —
    /// `scatter.remove` / `restore` address instances BY id.
    #[test]
    fn a_masked_chunk_is_still_recomputed_identically_and_keeps_its_ids() {
        let mut s = ground(64);
        let unmasked = chunk_instances(&s, (2, -3));
        s.density = Some(Density { rows: 2, data: vec![0.0, 1.0, 0.0, 1.0] });
        let a = chunk_instances(&s, (2, -3));
        let b = chunk_instances(&s, (2, -3));
        assert_eq!(a, b, "the same masked chunk gave two different answers");
        assert!(!a.is_empty() && a.len() < unmasked.len(), "the mask actually removed some");
        for inst in &a {
            let same = unmasked.iter().find(|u| u.id == inst.id).expect("id survives masking");
            assert_eq!(same.pos, inst.pos, "a survivor did not move because a neighbour went");
        }
    }

    /// …and a different seed gives a different forest. A generator that
    /// ignored its seed would pass the test above and be useless.
    #[test]
    fn a_different_seed_is_a_different_forest() {
        let a = chunk_instances(&ground(24), (0, 0));
        let mut other = ground(24);
        other.seed = 0xBEEF;
        let b = chunk_instances(&other, (0, 0));
        assert_ne!(a[0].pos, b[0].pos, "the seed did nothing");
        assert_ne!(a[0].id, b[0].id, "ids must not collide across seeds");
    }

    /// Ids are unique within a chunk and stable across chunks. A duplicate id
    /// would make "harvest this one" remove two.
    #[test]
    fn instance_ids_are_unique() {
        let s = ground(64);
        let mut seen = HashSet::new();
        for key in [(0, 0), (1, 0), (0, 1), (-5, 9)] {
            for i in chunk_instances(&s, key) {
                assert!(seen.insert(i.id), "duplicate instance id {}", i.id);
            }
        }
    }

    /// Position, scale and rotation come from INDEPENDENT streams. Deriving
    /// them by shifting one value correlates them, and a forest whose big trees
    /// are all in one corner reads as a bug nobody can name.
    #[test]
    fn placement_and_scale_are_uncorrelated() {
        let s = ground(400);
        let inst = chunk_instances(&s, (2, 2));
        let mid = inst.iter().map(|i| i.pos.x).sum::<f64>() / inst.len() as f64;
        let (mut near_big, mut near_n, mut far_big, mut far_n) = (0.0, 0, 0.0, 0);
        for i in &inst {
            if i.pos.x < mid {
                near_big += i.scale as f64;
                near_n += 1;
            } else {
                far_big += i.scale as f64;
                far_n += 1;
            }
        }
        let (a, b) = (near_big / near_n as f64, far_big / far_n as f64);
        assert!(
            (a - b).abs() < 0.08,
            "scale tracks position: mean {a:.3} one side, {b:.3} the other"
        );
    }

    /// Removal is by ID and it STICKS — that is the whole reason ids are stable
    /// across a stream-out and back in. A game that had to remember positions
    /// would be storing floats that came from a chain of arithmetic.
    #[test]
    fn a_removed_instance_stays_gone_when_the_chunk_streams_back_in() {
        let mut s = ground(24);
        let before = chunk_instances(&s, (1, 1));
        let victim = before[3].id;
        s.removed.insert(victim);

        let after = chunk_instances(&s, (1, 1));
        assert_eq!(after.len(), before.len() - 1);
        assert!(!after.iter().any(|i| i.id == victim), "the cut tree came back");
        // Everything else is untouched — removing one must not re-roll the rest.
        for i in before.iter().filter(|i| i.id != victim) {
            assert!(after.contains(i), "removing one instance moved another");
        }
    }

    /// LOD bands hand back a blend, not an index, because the pop at a band
    /// boundary is the thing everyone notices about scatter.
    #[test]
    fn bands_cross_fade_instead_of_popping() {
        let s = ground(8);
        assert_eq!(band_at(&s, 5.0), Some((0, 0.0)), "close in, fully near");
        let (band, t) = band_at(&s, 36.0).expect("still visible");
        assert_eq!(band, 0);
        assert!(t > 0.0 && t < 1.0, "should be mid-fade at the boundary, got {t}");
        let (band, _) = band_at(&s, 60.0).expect("still visible");
        assert_eq!(band, 1, "past the first band");
        assert!(band_at(&s, 200.0).is_none(), "past the last band it is gone");
    }

    /// Chunk selection is GENEROUS on purpose. A prop near a chunk's far corner
    /// must not be culled by the chunk it happens to live in — missing props at
    /// a seam is the classic scatter bug, and it only shows up as you walk.
    #[test]
    fn chunk_selection_covers_the_seams() {
        let s = ground(8);
        let eye = DVec3::new(0.4, 0.0, 0.4); // just inside chunk (0,0)
        let keys = chunks_near(&s, eye, 20.0);
        for k in [(0, 0), (-1, 0), (0, -1), (-1, -1), (1, 1)] {
            assert!(keys.contains(&k), "chunk {k:?} was culled at a seam");
        }
    }

    /// The knobs read as a look and one of them is the whole budget. `cost`
    /// says so in numbers, before a game ships it (`floptle/0071`).
    ///
    /// The two configurations here are the ones that actually happened: what
    /// shipped, and what it became once someone worked out what `lod` was
    /// really setting.
    #[test]
    fn cost_names_what_a_configuration_asks_for_every_frame() {
        // A level big enough that the view distance, not the region, decides.
        let big = |far: f32, chunk: f64, per: u32| ScatterSource {
            region: Region::Ground { center: DVec3::ZERO, half: [5_000.0, 5_000.0] },
            chunk,
            per_chunk: per,
            bands: vec![Band { asset: "rock.glb".into(), distance: far }],
            ..ground(per)
        };
        let shipped = cost(&big(700.0, 22.0, 26));
        let fixed = cost(&big(190.0, 34.0, 14));

        assert!(
            shipped.chunks > 3_000 && shipped.props > 90_000,
            "the shape that froze a game should read as thousands of chunks: {shipped:?}"
        );
        assert!(
            fixed.chunks < 200 && fixed.props < 3_000,
            "the fixed one should read as hundreds: {fixed:?}"
        );
        // An order of magnitude between two configurations a reasonable person
        // writes is the whole reason this number has to be visible.
        assert!(shipped.props > fixed.props * 30, "{shipped:?} vs {fixed:?}");
        assert_eq!(fixed.props, fixed.chunks * 14, "props is chunks x perChunk");

        // …and it really is the SQUARE of the distance. Doubling `lod` is
        // getting on for four times the work, which is the fact the knob's name
        // hides. (Just under four: the sweep carries a fixed couple of chunks
        // of slack, which weighs more on the smaller of the two.)
        let (a, b) = (cost(&big(200.0, 20.0, 1)), cost(&big(400.0, 20.0, 1)));
        let ratio = b.chunks as f64 / a.chunks as f64;
        assert!((3.0..4.2).contains(&ratio), "doubling the distance cost {ratio:.2}x, not ~4x");
    }

    /// On a body SMALLER than the view distance, residency saturates at the
    /// body. It used to keep growing: 700 m of `lod` on a 107 m planet swept
    /// 4,489 keys that resolved to 174 distinct chunks, and piled three
    /// quarters of its props on a cube-face seam (`floptle/0071`).
    #[test]
    fn a_planet_smaller_than_the_view_distance_does_not_keep_costing_more() {
        let planet = |far: f32| ScatterSource {
            region: Region::Sphere { center: DVec3::ZERO, radius: 107.0 },
            chunk: 22.0,
            per_chunk: 26,
            bands: vec![Band { asset: "rock.glb".into(), distance: far }],
            ..ground(26)
        };
        let near = cost(&planet(190.0));
        let silly = cost(&planet(700.0));
        assert_eq!(
            near, silly,
            "asking to see 700 m of a 214 m planet cost more than asking to see 190"
        );
        assert!(silly.chunks < 200, "the whole planet is {} chunks", silly.chunks);

        // And the props are real places, not the same rock drawn over and over.
        let eye = DVec3::new(0.0, 107.0, 0.0);
        let s = planet(700.0);
        let mut pos: Vec<String> = chunks_near(&s, eye, 700.0)
            .iter()
            .flat_map(|k| chunk_instances(&s, *k))
            .map(|i| format!("{:.2},{:.2},{:.2}", i.pos.x, i.pos.y, i.pos.z))
            .collect();
        let total = pos.len();
        pos.sort();
        pos.dedup();
        assert!(
            pos.len() * 10 > total * 9,
            "{} of {total} props are duplicates piled on a seam",
            total - pos.len()
        );
    }

    /// …and it counts the sweep rather than restating it. A formula beside
    /// `chunks_near` is a second definition of residency, and the two would
    /// drift the first time either changed.
    #[test]
    fn cost_counts_the_same_chunks_the_draw_walks() {
        let s = ground(9);
        let eye = DVec3::ZERO;
        assert_eq!(
            cost(&s).chunks,
            chunks_near(&s, eye, s.range() as f64).len() as u64,
            "cost and the real sweep disagree"
        );
        // A source with no bands costs nothing rather than dividing by zero.
        let mut empty = s.clone();
        empty.bands.clear();
        assert_eq!(cost(&empty), Cost::default());
    }

    /// The sweep comes back NEAREST FIRST, and that order is load-bearing: a
    /// draw budget cuts the tail of this list, and the tail has to be the
    /// horizon rather than whichever chunks a nested loop reached last. A
    /// budget that drops the props at your feet is worse than no budget.
    #[test]
    fn the_sweep_is_ordered_nearest_first() {
        for s in [ground(8), ScatterSource {
            region: Region::Sphere { center: DVec3::ZERO, radius: 300.0 },
            chunk: 30.0,
            ..ground(8)
        }] {
            let eye = match s.region {
                Region::Sphere { radius, .. } => DVec3::new(0.0, radius, 0.0),
                _ => DVec3::new(5.0, 0.0, -3.0),
            };
            let keys = chunks_near(&s, eye, s.range() as f64);
            assert!(keys.len() > 4, "not much of a sweep to order");
            // Measured from the eye's chunk, which is what the sweep is a
            // function of — within half a chunk of the eye, and stable as it
            // moves, which is the trade this makes on purpose.
            let from = chunk_center(&s, eye_chunk(&s, eye));
            let d: Vec<f64> =
                keys.iter().map(|k| (chunk_center(&s, *k) - from).length()).collect();
            assert!(
                d.windows(2).all(|w| w[0] <= w[1] + 1e-9),
                "the sweep is not nearest-first: {:?}",
                &d[..d.len().min(8)]
            );
        }
    }

    /// The square sweep is cut to a disc. A corner of the swept square is √2
    /// range away and can hold nothing visible — a fifth of the chunks walked
    /// every frame for nothing.
    #[test]
    fn the_corners_of_a_square_sweep_are_not_walked() {
        let mut s = ground(8);
        s.region = Region::Ground { center: DVec3::ZERO, half: [5_000.0, 5_000.0] };
        s.chunk = 10.0;
        s.bands = vec![Band { asset: "a".into(), distance: 400.0 }];
        let keys = chunks_near(&s, DVec3::ZERO, 400.0);
        let n = (410.0f64 / 10.0).ceil() as i64;
        let square = ((2 * n + 1) * (2 * n + 1)) as usize;
        assert!(keys.len() < square * 9 / 10, "{} of {square} kept — no cull", keys.len());
        // …and nothing that could hold a visible prop was cut. Every key within
        // the honest reach is still there.
        for x in -n..=n {
            for z in -n..=n {
                let c = chunk_center(&s, (x, z));
                if c.length() <= 400.0 {
                    assert!(keys.contains(&(x, z)), "chunk {x},{z} was culled inside range");
                }
            }
        }
    }

    /// The key set changes when the eye crosses a chunk boundary, not when the
    /// frame advances — which is what lets the draw skip the sweep entirely
    /// while a player stands still.
    #[test]
    fn the_eyes_chunk_only_changes_at_a_boundary() {
        let s = ground(8); // chunk = 16
        let a = eye_chunk(&s, DVec3::new(1.0, 0.0, 1.0));
        assert_eq!(a, eye_chunk(&s, DVec3::new(15.9, 40.0, 2.0)), "still the same chunk");
        assert_ne!(a, eye_chunk(&s, DVec3::new(16.1, 0.0, 1.0)), "crossed into the next one");
        // …and the sweep from anywhere in one chunk is the SAME LIST, order
        // included. That is not tidiness: the draw caches this list until the
        // eye crosses a boundary, so a sweep that drifted with sub-chunk
        // movement would make the cache quietly wrong instead of merely stale.
        assert_eq!(
            chunks_near(&s, DVec3::new(1.0, 0.0, 1.0), 40.0),
            chunks_near(&s, DVec3::new(14.0, 3.0, 14.0), 40.0),
            "the sweep moved without the eye leaving its chunk"
        );
        assert_ne!(
            chunks_near(&s, DVec3::new(1.0, 0.0, 1.0), 40.0),
            chunks_near(&s, DVec3::new(20.0, 0.0, 1.0), 40.0),
            "…but crossing a boundary does change it"
        );
    }

    /// A planet's props sit ON the sphere — every one of them, at the radius
    /// asked for. Cube-face projection rather than lat/long is what keeps the
    /// density even instead of piling everything at the poles.
    #[test]
    fn sphere_instances_land_on_the_surface() {
        let s = ScatterSource {
            region: Region::Sphere { center: DVec3::new(1000.0, 0.0, 0.0), radius: 600.0 },
            chunk: 40.0,
            ..ground(32)
        };
        let center = DVec3::new(1000.0, 0.0, 0.0);
        for key in [(0, 0), (2, 5), ((3 << 3) | 4, -2)] {
            for i in chunk_instances(&s, key) {
                let r = (i.pos - center).length();
                assert!((r - 600.0).abs() < 1e-6, "instance at radius {r}, wanted 600");
                // …and its up is the outward radial, so a tree stands off the
                // ground rather than leaning at whatever world +Y happens to be.
                let radial = (i.pos - center).normalize().as_vec3();
                assert!(i.up.dot(radial) > 0.999, "up is not the surface normal");
            }
        }
    }

    /// Surface alignment tilts THEN spins. The other order spins about world
    /// +Y and leaves a hillside's trees all facing the same way as they lean.
    #[test]
    fn surface_alignment_stands_a_prop_off_its_ground() {
        let i = Instance {
            id: 1,
            pos: DVec3::ZERO,
            up: Vec3::new(0.0, 0.6, 0.8).normalize(),
            yaw: 1.2,
            scale: 1.0,
            param: 0.0,
        };
        let local_up = i.rotation(Align::Surface) * Vec3::Y;
        assert!(local_up.dot(i.up) > 0.999, "the prop is not standing on its slope");
        // World alignment ignores the normal entirely.
        let flat = i.rotation(Align::World) * Vec3::Y;
        assert!(flat.dot(Vec3::Y) > 0.999);
    }

    /// A prop rolled outside the declared rectangle is REJECTED, not clamped.
    /// Clamping builds a wall of props along the boundary — visibly a bug, and
    /// the kind you fix by shrinking the region until you notice why.
    #[test]
    fn out_of_region_rolls_are_dropped_not_clamped() {
        let mut s = ground(200);
        s.region = Region::Ground { center: DVec3::ZERO, half: [8.0, 8.0] };
        let inst = chunk_instances(&s, (0, 0));
        assert!(!inst.is_empty(), "the chunk overlaps the region");
        let on_edge = inst.iter().filter(|i| (i.pos.x.abs() - 8.0).abs() < 0.01).count();
        assert_eq!(on_edge, 0, "{on_edge} props stacked on the region boundary");
        for i in &inst {
            assert!(i.pos.x.abs() <= 8.0 && i.pos.z.abs() <= 8.0);
        }
    }
}
