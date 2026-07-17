//! Deterministic noise + RNG — the ONE implementation Rust generators and the Lua
//! `math.noise`/`rng()` API share, so a planet generated in Rust and decoration
//! scattered from a script agree on every number, on every machine (no libm/crate
//! variance, no platform drift — a requirement for replicated procgen).

use crate::math::Vec3;

/// Deterministic xorshift32 — small, fast, plenty for gameplay (loot rolls, spawn
/// jitter, decoration scatter). NOT cryptographic. State is `pub` so hosts can
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
        let b = [p.x.floor() as i32, p.y.floor() as i32, p.z.floor() as i32];
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
}
