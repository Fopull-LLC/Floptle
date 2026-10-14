//! Deterministic noise + RNG — the one implementation Rust generators and the Lua
//! `math.noise`/`rng()` API share, so a planet generated in Rust and decoration
//! scattered from a script agree on every number, on every machine (no libm/crate
//! variance, no platform drift — a requirement for replicated procgen).

use crate::math::Vec3;

/// Deterministic xorshift32 — small, fast, plenty for gameplay (loot rolls, spawn
/// jitter, decoration scatter). not cryptographic. State is `pub` so hosts can
/// snapshot/replicate it.
#[derive(Clone, Copy, Debug)]
pub struct Rng {
    pub state: u32,
}

impl Rng {
    pub fn new(seed: u32) -> Self {
        // Zero is xorshift's fixed point — nudge it off.
        Self { state: if seed == 0 { 0x9E3779B9 } else { seed } }
    }

    #[inline]
    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        x
    }

    /// Uniform in `[0, 1)`.
    #[inline]
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u32() >> 8) as f64 / (1u32 << 24) as f64
    }

    /// Uniform in `[a, b)` (or `[b, a)` if reversed).
    pub fn range(&mut self, a: f64, b: f64) -> f64 {
        a + (b - a) * self.next_f64()
    }

    /// Uniform integer in `[a, b]` inclusive.
    pub fn int(&mut self, a: i64, b: i64) -> i64 {
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let span = (hi - lo) as u64 + 1;
        lo + (self.next_u32() as u64 % span) as i64
    }
}

/// `x.floor() as i32`, without the library call `floor` compiles to on a
/// target with no rounding instruction (baseline x86-64 has none), which was
/// the largest single cost of a noise sample. The same answer for every `f32`,
/// saturating where the cast does.
#[inline]
fn floor_i32(x: f32) -> i32 {
    let i = x as i32;
    if (i as f32) > x { i.saturating_sub(1) } else { i }
}

/// The lattice cells one [`Noise::fbm_cached`] call site last read, one per
/// octave.
#[derive(Clone, Copy, Debug, Default)]
pub struct FbmCache([Option<([i32; 3], [f32; 8])>; 10]);

/// Seeded 3D value noise (hash lattice, smoothstepped trilinear). Roughly `[-1, 1]`.
#[derive(Clone, Copy, Debug)]
pub struct Noise {
    pub seed: u32,
}

impl Noise {
    pub fn new(seed: u32) -> Self {
        Self { seed }
    }

    fn hash(&self, x: i32, y: i32, z: i32) -> f32 {
        let mut h = self
            .seed
            .wrapping_mul(0x9E3779B9)
            .wrapping_add((x as u32).wrapping_mul(0x85EB_CA6B))
            .wrapping_add((y as u32).wrapping_mul(0xC2B2_AE35))
            .wrapping_add((z as u32).wrapping_mul(0x27D4_EB2F));
        h ^= h >> 15;
        h = h.wrapping_mul(0x2C1B_3C6D);
        h ^= h >> 12;
        h = h.wrapping_mul(0x297A_2D39);
        h ^= h >> 15;
        (h as f32 / u32::MAX as f32) * 2.0 - 1.0
    }

    /// One octave of value noise at `p` (lattice cell = 1 unit). Scale the input to
    /// pick a frequency.
    pub fn value(&self, p: Vec3) -> f32 {
        let b = [floor_i32(p.x), floor_i32(p.y), floor_i32(p.z)];
        let f = Vec3::new(p.x - b[0] as f32, p.y - b[1] as f32, p.z - b[2] as f32);
        let s = Vec3::new(
            f.x * f.x * (3.0 - 2.0 * f.x),
            f.y * f.y * (3.0 - 2.0 * f.y),
            f.z * f.z * (3.0 - 2.0 * f.z),
        );
        let c = |dx: i32, dy: i32, dz: i32| self.hash(b[0] + dx, b[1] + dy, b[2] + dz);
        let l = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let x00 = l(c(0, 0, 0), c(1, 0, 0), s.x);
        let x10 = l(c(0, 1, 0), c(1, 1, 0), s.x);
        let x01 = l(c(0, 0, 1), c(1, 0, 1), s.x);
        let x11 = l(c(0, 1, 1), c(1, 1, 1), s.x);
        l(l(x00, x10, s.y), l(x01, x11, s.y), s.z)
    }

    /// Fractal Brownian motion: `octaves` layers, each rotated (an axis-aligned
    /// value-noise lattice stamps boxy features — the first Solar planetoid had
    /// literal terraces before the per-octave rotation) and offset. Roughly `[-1, 1]`.
    pub fn fbm(&self, p: Vec3, octaves: u32) -> f32 {
        let rot = |v: Vec3| {
            Vec3::new(
                0.36 * v.x - 0.80 * v.y + 0.48 * v.z,
                0.80 * v.x + 0.52 * v.y + 0.30 * v.z,
                -0.48 * v.x + 0.30 * v.y + 0.82 * v.z,
            )
        };
        let (mut amp, mut sum, mut norm) = (1.0f32, 0.0f32, 0.0f32);
        let mut q = p;
        for i in 0..octaves.clamp(1, 10) {
            sum += self.value(q + Vec3::splat(i as f32 * 19.19)) * amp;
            norm += amp;
            amp *= 0.5;
            q = rot(q) * 2.03;
        }
        sum / norm.max(1e-6)
    }

    /// Ridged noise in `[0, 1]`: each octave folded about zero and turned
    /// upside down, `(1 - |n|)^sharpness`, so its zero lines become crests.
    /// Mountain ranges, where `fbm` makes rolling hills; the octaves turn and
    /// scale as `fbm`'s do. Higher `sharpness` makes thinner, steeper ridges.
    pub fn ridged(&self, p: Vec3, octaves: u32, sharpness: f32) -> f32 {
        let rot = |v: Vec3| {
            Vec3::new(
                0.36 * v.x - 0.80 * v.y + 0.48 * v.z,
                0.80 * v.x + 0.52 * v.y + 0.30 * v.z,
                -0.48 * v.x + 0.30 * v.y + 0.82 * v.z,
            )
        };
        let (mut amp, mut sum, mut norm) = (1.0f32, 0.0f32, 0.0f32);
        let mut q = p;
        for i in 0..octaves.clamp(1, 10) {
            let n = self.value(q + Vec3::splat(i as f32 * 19.19)).abs().min(1.0);
            sum += (1.0 - n).powf(sharpness) * amp;
            norm += amp;
            amp *= 0.5;
            q = rot(q) * 2.03;
        }
        sum / norm.max(1e-6)
    }

    /// [`Self::fbm`], reusing lattice values `cache` already holds.
    ///
    /// The same number, bit for bit: only the hashing is skipped, never the
    /// blend. Worth it when consecutive points mostly share lattice cells — a
    /// generator walking a block of voxels, where most octaves' cells are
    /// several voxels across. One cache per call site, since each call site
    /// walks its own lattice.
    pub fn fbm_cached(&self, p: Vec3, octaves: u32, cache: &mut FbmCache) -> f32 {
        let rot = |v: Vec3| {
            Vec3::new(
                0.36 * v.x - 0.80 * v.y + 0.48 * v.z,
                0.80 * v.x + 0.52 * v.y + 0.30 * v.z,
                -0.48 * v.x + 0.30 * v.y + 0.82 * v.z,
            )
        };
        let (mut amp, mut sum, mut norm) = (1.0f32, 0.0f32, 0.0f32);
        let mut q = p;
        for i in 0..octaves.clamp(1, 10) {
            sum += self.value_cached(q + Vec3::splat(i as f32 * 19.19), &mut cache.0[i as usize]) * amp;
            norm += amp;
            amp *= 0.5;
            q = rot(q) * 2.03;
        }
        sum / norm.max(1e-6)
    }

    /// [`Self::value`] with the cell's eight lattice values kept in `cell`.
    fn value_cached(&self, p: Vec3, cell: &mut Option<([i32; 3], [f32; 8])>) -> f32 {
        let b = [floor_i32(p.x), floor_i32(p.y), floor_i32(p.z)];
        let f = Vec3::new(p.x - b[0] as f32, p.y - b[1] as f32, p.z - b[2] as f32);
        let s = Vec3::new(
            f.x * f.x * (3.0 - 2.0 * f.x),
            f.y * f.y * (3.0 - 2.0 * f.y),
            f.z * f.z * (3.0 - 2.0 * f.z),
        );
        let c = match cell {
            Some((k, c)) if *k == b => *c,
            _ => {
                let h = |dx: i32, dy: i32, dz: i32| self.hash(b[0] + dx, b[1] + dy, b[2] + dz);
                let c = [h(0, 0, 0), h(1, 0, 0), h(0, 1, 0), h(1, 1, 0), h(0, 0, 1), h(1, 0, 1), h(0, 1, 1), h(1, 1, 1)];
                *cell = Some((b, c));
                c
            }
        };
        let l = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let x00 = l(c[0], c[1], s.x);
        let x10 = l(c[2], c[3], s.x);
        let x01 = l(c[4], c[5], s.x);
        let x11 = l(c[6], c[7], s.x);
        l(l(x00, x10, s.y), l(x01, x11, s.y), s.z)
    }

    /// Bounds on one octave of [`Self::value`] over the box `[lo, hi]`.
    ///
    /// Inside one lattice cell the noise is a trilinear blend of eight lattice
    /// values whose weights are smoothsteps, each monotone in its own axis, so
    /// its least and greatest values over a box in that cell are at the box's
    /// corners. A box crossing cells is cut at the cell walls and each piece
    /// bounded the same way; one spanning more than a few cells gets the whole
    /// range.
    pub fn value_bounds(&self, lo: Vec3, hi: Vec3) -> (f32, f32) {
        let (b0, b1) = (lo.floor(), hi.floor());
        let n = (b1 - b0 + Vec3::ONE).to_array();
        if n.iter().any(|&a| !a.is_finite()) || n[0] * n[1] * n[2] > 27.0 {
            return (-1.0, 1.0);
        }
        // One piece per cell the box reaches, cut at the cell walls; the noise
        // is continuous across a wall, so the shared face bounds both sides.
        let (mut mn, mut mx) = (f32::INFINITY, f32::NEG_INFINITY);
        for cz in b0.z as i32..=b1.z as i32 {
            for cy in b0.y as i32..=b1.y as i32 {
                for cx in b0.x as i32..=b1.x as i32 {
                    let cell = Vec3::new(cx as f32, cy as f32, cz as f32);
                    let (l, h) = (lo.max(cell), hi.min(cell + Vec3::ONE));
                    for k in 0..8 {
                        let c = Vec3::new(
                            if k & 1 == 0 { l.x } else { h.x },
                            if k & 2 == 0 { l.y } else { h.y },
                            if k & 4 == 0 { l.z } else { h.z },
                        );
                        let v = self.value(c);
                        mn = mn.min(v);
                        mx = mx.max(v);
                    }
                }
            }
        }
        (mn, mx)
    }

    /// Bounds on [`Self::fbm`] over the box `[lo, hi]`: every value it takes
    /// anywhere in the box is in the returned range.
    ///
    /// Each octave is bounded over the box carried through that octave's own
    /// rotation and scale (the box around the turned box, so a little looser
    /// than the octave's true range), and the octaves' bounds are summed with
    /// the weights `fbm` gives them. A generator asks this to learn that a
    /// whole block of voxels is solid rock or open air without evaluating any
    /// of them.
    pub fn fbm_bounds(&self, lo: Vec3, hi: Vec3, octaves: u32) -> (f32, f32) {
        // The rotation `fbm` turns each octave by, as rows.
        const R: [[f32; 3]; 3] = [[0.36, -0.80, 0.48], [0.80, 0.52, 0.30], [-0.48, 0.30, 0.82]];
        let (mut amp, mut lo_sum, mut hi_sum, mut norm) = (1.0f32, 0.0f32, 0.0f32, 0.0f32);
        let (mut c, mut h) = ((lo + hi) * 0.5, (hi - lo).abs() * 0.5);
        for i in 0..octaves.clamp(1, 10) {
            // A hair of slack, so rounding in the turned box can only widen it.
            let pad = h + Vec3::splat(1e-4 + c.abs().max_element() * 1e-6);
            let off = Vec3::splat(i as f32 * 19.19);
            let (vl, vh) = self.value_bounds(c - pad + off, c + pad + off);
            lo_sum += vl * amp;
            hi_sum += vh * amp;
            norm += amp;
            amp *= 0.5;
            let turn = |v: Vec3, abs: bool| {
                let row = |r: [f32; 3]| {
                    if abs {
                        r[0].abs() * v.x + r[1].abs() * v.y + r[2].abs() * v.z
                    } else {
                        r[0] * v.x + r[1] * v.y + r[2] * v.z
                    }
                };
                Vec3::new(row(R[0]), row(R[1]), row(R[2]))
            };
            c = turn(c, false) * 2.03;
            h = turn(h, true) * 2.03;
        }
        let norm = norm.max(1e-6);
        // Dividing can round either way; widen by an ulp's worth.
        (lo_sum / norm - 1e-6, hi_sum / norm + 1e-6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Determinism is the whole point: fixed seeds must produce these exact values
    /// forever, on every platform. If this test breaks, saved worlds and replicated
    /// procgen break with it — change the constants only with a migration story.
    #[test]
    fn noise_and_rng_are_deterministic_forever() {
        let mut r = Rng::new(7);
        let a: Vec<u32> = (0..4).map(|_| r.next_u32()).collect();
        let mut r2 = Rng::new(7);
        let b: Vec<u32> = (0..4).map(|_| r2.next_u32()).collect();
        assert_eq!(a, b);
        assert_ne!(a[0], a[1]);
        // Distribution sanity, not statistics: values land in range.
        let mut r = Rng::new(123);
        for _ in 0..100 {
            let v = r.next_f64();
            assert!((0.0..1.0).contains(&v));
            let i = r.int(-3, 3);
            assert!((-3..=3).contains(&i));
        }
        let n = Noise::new(7);
        let v = n.value(Vec3::new(1.3, 2.7, -0.4));
        let f = n.fbm(Vec3::new(1.3, 2.7, -0.4), 4);
        assert!((-1.2..=1.2).contains(&v) && (-1.2..=1.2).contains(&f));
        // Seeds matter.
        assert_ne!(
            Noise::new(7).value(Vec3::splat(0.5)),
            Noise::new(8).value(Vec3::splat(0.5))
        );
        // Continuity: nearby points give nearby values (no lattice pops).
        let d = (n.value(Vec3::new(1.30, 2.7, -0.4)) - n.value(Vec3::new(1.31, 2.7, -0.4))).abs();
        assert!(d < 0.1, "noise jumped {d} over 0.01 units");
    }

    /// **Every value fbm takes in a box is inside `fbm_bounds` for that box.**
    /// A generator skips evaluating a block of voxels on this answer, so a
    /// bound that is ever too tight carves a wrong wall into a planet. Boxes of
    /// every size from a sliver to several lattice cells, at several
    /// frequencies, sampled inside and on their corners.
    #[test]
    fn fbm_bounds_hold_everything_inside_the_box() {
        let n = Noise::new(800921623);
        let mut r = Rng::new(11);
        let mut tight = 0usize;
        for case in 0..4000 {
            let c = Vec3::new(r.range(-60.0, 60.0) as f32, r.range(-60.0, 60.0) as f32, r.range(-60.0, 60.0) as f32);
            let size = [0.01, 0.1, 0.4, 1.0, 3.0][case % 5];
            let h = Vec3::new(r.next_f64() as f32, r.next_f64() as f32, r.next_f64() as f32) * size;
            let oct = 1 + (case / 5 % 5) as u32;
            let (lo, hi) = n.fbm_bounds(c - h, c + h, oct);
            assert!(lo <= hi, "{lo} > {hi}");
            for k in 0..40 {
                let t = Vec3::new(r.next_f64() as f32, r.next_f64() as f32, r.next_f64() as f32);
                let t = if k < 8 {
                    Vec3::new((k & 1) as f32, ((k >> 1) & 1) as f32, ((k >> 2) & 1) as f32)
                } else {
                    t
                };
                let p = c - h + 2.0 * h * t;
                let v = n.fbm(p, oct);
                assert!(v >= lo && v <= hi, "fbm {v} outside [{lo}, {hi}] at {p:?} (box {c:?} ± {h:?}, {oct} octaves)");
            }
            if size <= 0.1 && oct <= 2 && hi - lo < 0.5 {
                tight += 1;
            }
        }
        // And small boxes get a useful answer, not the whole range.
        assert!(tight > 600, "only {tight} of 640 small one- and two-octave boxes were bounded usefully");
    }

    /// `fbm_cached` is `fbm`, bit for bit, along a walk that keeps landing in
    /// cells the cache holds and one that jumps between them.
    #[test]
    fn cached_fbm_is_fbm() {
        let n = Noise::new(31337);
        let mut cache = FbmCache::default();
        let mut r = Rng::new(5);
        for k in 0..20000 {
            let p = if k % 3 == 0 {
                Vec3::new(r.range(-500.0, 500.0) as f32, r.range(-500.0, 500.0) as f32, r.range(-500.0, 500.0) as f32)
            } else {
                Vec3::new(k as f32 * 0.013, (k / 50) as f32 * 0.21, -3.7)
            };
            let oct = 1 + (k % 6) as u32;
            assert_eq!(n.fbm_cached(p, oct, &mut cache).to_bits(), n.fbm(p, oct).to_bits(), "{p:?} × {oct}");
        }
    }

    /// `floor_i32` is `floor() as i32` for every kind of `f32`: either side of
    /// zero, on and just off the integers, past the `i32` range, and NaN.
    #[test]
    fn floor_i32_is_floor() {
        let mut xs = vec![0.0f32, -0.0, 0.5, -0.5, 1.0, -1.0, 1e-30, -1e-30, 3e9, -3e9, f32::MAX, f32::MIN, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 2147483520.0, -2147483648.0, -2147483904.0];
        let mut r = Rng::new(3);
        for _ in 0..100000 {
            let v = r.range(-1e5, 1e5) as f32;
            xs.extend([v, v.floor(), f32::from_bits(v.floor().to_bits() + 1), f32::from_bits(v.floor().to_bits().wrapping_sub(1))]);
        }
        for x in xs {
            assert_eq!(floor_i32(x), x.floor() as i32, "{x:?}");
        }
    }
}
