//! The kinematic capsule character controller: grounded movement, stepping,
//! jumping, and gravity-aligned orientation.

use floptle_core::math::Vec3;

use crate::world::PhysicsWorld;

/// A kinematic capsule character controller (ADR-0014, the "cool movement"): moved by
/// input + gravity, it slides along surfaces, snaps to ground, respects a slope limit,
/// and keeps its `up` aligned to −gravity — so it runs around spherical planets and up
/// swirling fractal walls. `pos` is the FOOT (bottom of the capsule). It queries the
/// world read-only (it's not a dynamic body in the solver).
#[derive(Clone, Copy, Debug)]
pub struct Character {
    pub pos: Vec3,
    pub vel: Vec3,
    /// Current up direction (aligned to −gravity each step).
    pub up: Vec3,
    pub radius: f32,
    /// Total standing height (clamped to ≥ 2·radius).
    pub height: f32,
    pub move_speed: f32,
    /// Steepest surface (angle from `up`, radians) the character can stand on; steeper
    /// ground doesn't ground it, so it slides off.
    pub slope_limit: f32,
    pub grounded: bool,
}

impl Character {
    pub fn new(pos: Vec3, radius: f32, height: f32) -> Self {
        Self {
            pos,
            vel: Vec3::ZERO,
            up: Vec3::Y,
            radius,
            height: height.max(2.0 * radius),
            move_speed: 4.0,
            slope_limit: 50f32.to_radians(),
            grounded: false,
        }
    }

    /// The two end-sphere centers of the capsule (bottom, top) given the current `up`.
    fn spheres(&self) -> [Vec3; 2] {
        [self.pos + self.up * self.radius, self.pos + self.up * (self.height - self.radius)]
    }

    /// Advance one FIXED step. `move_input` is a desired direction in world space (its
    /// magnitude 0..1 scales speed); only its component along the ground tangent plane
    /// is used, so input perpendicular to a wall/planet just walks along it.
    pub fn update(&mut self, world: &PhysicsWorld, move_input: Vec3, dt: f32) {
        let dt = dt.clamp(0.0, 0.1);
        let cols = &world.colliders;

        // 1. "Up" = −gravity at the foot (so we orient to planets / fractal walls).
        let g = world.gravity.accel_at(self.pos, cols);
        if g.length_squared() > 1e-10 {
            self.up = (-g).normalize();
        }

        // 2. Velocity: keep the vertical (fall) part, set the horizontal part from the
        //    input projected onto the tangent plane.
        self.vel += g * dt;
        let v_up = self.vel.dot(self.up);
        let tangent = move_input - self.up * move_input.dot(self.up);
        let speed = self.move_speed * move_input.length().min(1.0);
        let horiz = tangent.try_normalize().map(|d| d * speed).unwrap_or(Vec3::ZERO);
        self.vel = horiz + self.up * v_up;

        // 3. Integrate.
        self.pos += self.vel * dt;

        // 4. Depenetrate the capsule from every collider (both end spheres, relaxed).
        for _ in 0..3 {
            for shape in cols {
                for c in self.spheres() {
                    let pen = self.radius - shape.distance(c);
                    if pen > 0.0 {
                        let n = shape.normal(c);
                        self.pos += n * pen;
                        let vn = self.vel.dot(n);
                        if vn < 0.0 {
                            self.vel -= n * vn; // cancel into-surface velocity
                        }
                    }
                }
            }
        }

        // 5. Ground check + snap: the foot sphere; ground only if a near surface
        //    opposes gravity within the slope limit.
        self.grounded = false;
        let bottom = self.pos + self.up * self.radius;
        let mut best: Option<(f32, Vec3)> = None; // (gap, normal)
        for shape in cols {
            let gap = shape.distance(bottom) - self.radius; // 0 = touching
            if gap < 0.25 {
                let n = shape.normal(bottom);
                if n.dot(self.up) > self.slope_limit.cos()
                    && best.is_none_or(|(bg, _)| gap < bg)
                {
                    best = Some((gap, n));
                }
            }
        }
        if let Some((gap, _)) = best {
            if (0.0..0.25).contains(&gap) {
                self.pos -= self.up * gap; // snap down a small gap (follow curved ground)
            }
            let v_up = self.vel.dot(self.up);
            if v_up < 0.0 {
                self.vel -= self.up * v_up; // stop falling
            }
            self.grounded = true;
        }
    }
}
