//! Dynamic rigid bodies: shape, integration state, material response
//! (restitution/friction), and contact resolution against collision shapes.

use floptle_core::math::Vec3;


/// The collision shape of a dynamic body.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BodyShape {
    /// A sphere of the body's `radius`.
    Sphere,
    /// A capsule: `radius` thick, with `half_height` from the center to each end-sphere
    /// center, aligned to the body's `up` (kept along −gravity, so it stands upright).
    Capsule { half_height: f32 },
    /// A world-axis-aligned box with the given half-extents. Depenetrated by sampling its
    /// 8 corners + center as points against the world (so a crate rests flat on a floor).
    /// Not orientation-tracked in the solver — intended for crates/platforms under level
    /// gravity, not free-tumbling rotation.
    Box { half: Vec3 },
}

/// A dynamic body — a sphere or capsule integrated + depenetrated each step.
#[derive(Clone, Copy, Debug)]
pub struct Body {
    pub pos: Vec3,
    /// `pos` as of the START of the most recent fixed step. Rendering interpolates
    /// between the two by the accumulator's leftover fraction, so on-screen motion
    /// is smooth even though the sim advances in whole 1/120 s steps (without this,
    /// frames alternate between covering 1 and 2 steps — visible micro-jitter).
    pub prev_pos: Vec3,
    pub vel: Vec3,
    pub radius: f32,
    pub shape: BodyShape,
    /// Capsule axis, kept aligned to −gravity each step.
    pub up: Vec3,
    /// Bounciness: 0 = no bounce, 1 = perfectly elastic.
    pub restitution: f32,
    /// Surface friction: 0 = frictionless ice, 1 = no sliding.
    pub friction: f32,
    /// Whether the gravity field pulls on this body (false = floats, still collides).
    pub use_gravity: bool,
    /// Freeze world-axis translation (x, y, z) — e.g. lock Z for a 2.5D game.
    pub lock_pos: [bool; 3],
    /// Set each step when the body is resting on a surface that opposes gravity.
    pub grounded: bool,
    /// The contact normal from the most recent resolved collision this step (telegraph).
    pub contact: Option<Vec3>,
    /// The position restored on locked axes: captured at spawn, re-captured per
    /// axis at the moment its lock engages (so locking mid-play freezes in place).
    pub(crate) home: Vec3,
    /// Inactive bodies are skipped by the step AND the transform writeback —
    /// a networked CLIENT deactivates server-authoritative bodies so local
    /// physics never fights the interpolated snapshots driving their
    /// transforms (`docs/netcode-design.md` §6). Default true.
    pub active: bool,
}

impl Body {
    pub fn sphere(pos: Vec3, radius: f32) -> Self {
        Self {
            pos,
            prev_pos: pos,
            vel: Vec3::ZERO,
            radius,
            shape: BodyShape::Sphere,
            up: Vec3::Y,
            restitution: 0.0,
            friction: 0.3,
            use_gravity: true,
            lock_pos: [false; 3],
            grounded: false,
            contact: None,
            home: pos,
            active: true,
        }
    }

    /// A capsule body of total standing `height` (clamped to ≥ 2·radius).
    pub fn capsule(pos: Vec3, radius: f32, height: f32) -> Self {
        let half = (height.max(2.0 * radius) * 0.5 - radius).max(0.0);
        Self { shape: BodyShape::Capsule { half_height: half }, ..Self::sphere(pos, radius) }
    }

    /// A box body with the given world-axis half-extents (a crate / falling platform).
    pub fn boxx(pos: Vec3, half: Vec3) -> Self {
        let h = half.abs().max(Vec3::splat(1e-3));
        Self { shape: BodyShape::Box { half: h }, radius: h.min_element(), ..Self::sphere(pos, h.min_element()) }
    }

    /// Total standing height (tip to tip): `2·radius` for a sphere, the full capsule
    /// length for a capsule, `2·half.y` for a box.
    pub(crate) fn height(&self) -> f32 {
        match self.shape {
            BodyShape::Sphere => 2.0 * self.radius,
            BodyShape::Capsule { half_height } => 2.0 * (half_height + self.radius),
            BodyShape::Box { half } => 2.0 * half.y,
        }
    }

    /// The body's collision sample points + count + the sphere radius inflating each one:
    /// 1 point (sphere), the 2 end-sphere centers (capsule), or the 8 corners + center
    /// (box, sampled as zero-radius points). The depenetration loop pushes each point that
    /// has sunk inside a collider back out along the collider normal.
    pub(crate) fn sample_centers(&self) -> ([Vec3; 9], usize, f32) {
        let mut a = [self.pos; 9];
        match self.shape {
            BodyShape::Sphere => (a, 1, self.radius),
            BodyShape::Capsule { half_height } => {
                a[0] = self.pos - self.up * half_height;
                a[1] = self.pos + self.up * half_height;
                (a, 2, self.radius)
            }
            BodyShape::Box { half } => {
                let mut n = 0;
                for &sx in &[-1.0f32, 1.0] {
                    for &sy in &[-1.0f32, 1.0] {
                        for &sz in &[-1.0f32, 1.0] {
                            a[n] = self.pos + Vec3::new(sx * half.x, sy * half.y, sz * half.z);
                            n += 1;
                        }
                    }
                }
                a[8] = self.pos; // center, so a deeply-buried box still resolves
                (a, 9, 0.0)
            }
        }
    }
}

/// A resolved contact this step — for collision telegraphing / events.
#[derive(Clone, Copy, Debug)]
pub struct Contact {
    pub body: usize,
    pub point: Vec3,
    pub normal: Vec3,
}

pub(crate) fn axis(v: Vec3, i: usize) -> f32 {
    match i {
        0 => v.x,
        1 => v.y,
        _ => v.z,
    }
}
pub(crate) fn set_axis(v: &mut Vec3, i: usize, val: f32) {
    match i {
        0 => v.x = val,
        1 => v.y = val,
        _ => v.z = val,
    }
}
