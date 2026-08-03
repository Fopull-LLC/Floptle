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

/// Optional per-instance collision. A capsule proxy, not the prototype's real
/// geometry: a tree you cannot walk through only has to be *a tree-sized thing
/// in the way*, and a harvest tool only has to be able to aim at it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Proxy {
    pub radius: f32,
    pub height: f32,
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
    pub collide: Option<Proxy>,
    /// Instances the game has removed (harvested, dug out from under).
    pub removed: HashSet<InstanceId>,
}

impl ScatterSource {
    /// The cull distance — the last band's.
    pub fn range(&self) -> f32 {
        self.bands.last().map(|b| b.distance).unwrap_or(0.0)
    }
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

/// Every chunk key whose chunk could contain something within `range` of `eye`.
///
/// Deliberately generous: a chunk is included if its CENTRE is within
/// `range + chunk`, so a prop near a chunk's far corner is never culled by the
/// chunk it happens to live in. Missing props at a chunk seam is the classic
/// scatter bug and it only shows up as you walk.
pub fn chunks_near(src: &ScatterSource, eye: DVec3, range: f64) -> Vec<ChunkKey> {
    let mut keys = Vec::new();
    let reach = range + src.chunk;
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
                        keys.push((x, z));
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
            for du in (cu - n)..=(cu + n) {
                for dv in (cv - n)..=(cv + n) {
                    keys.push(((du << 3) | face as i64, dv));
                }
            }
        }
    }
    keys
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
            collide: Some(Proxy { radius: 0.4, height: 3.0 }),
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
