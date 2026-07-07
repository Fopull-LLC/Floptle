//! The collision world: anchored (large-world-safe) static colliders, the
//! dynamic body set, fixed-step advance, and raycasts.

use floptle_core::math::{DVec3, Vec3};

use crate::body::{axis, set_axis, Body, Contact};
use crate::gravity::{GravityField, GravitySource};
use crate::shapes::CollisionShape;

/// A collider plus the world-space frame its geometry is expressed in (ADR-0015).
///
/// The sim runs **origin-relative**: bodies and queries use small coordinates near
/// `world.origin`, never absolute world positions. Each collider's data is baked in
/// its own frame (`anchor`, full `f64`); `offset = anchor − world.origin` is cached
/// as `f32` and recomputed *from the `f64` anchor* on every rebase — so precision
/// near the action never depends on how far the content sits from the world origin,
/// and repeated rebases accumulate zero error into the geometry.
pub struct AnchoredCollider {
    pub shape: Box<dyn CollisionShape>,
    /// World-space anchor of the frame `shape`'s data is expressed in.
    pub anchor: DVec3,
    /// Cached `(anchor − world.origin)` as f32; queries subtract it from the probe.
    offset: Vec3,
}

impl AnchoredCollider {
    /// A collider whose data is in ABSOLUTE world coordinates (anchor = 0) — the
    /// right frame for data that's already near the world origin, and for tests.
    pub fn world(shape: Box<dyn CollisionShape>) -> Self {
        Self { shape, anchor: DVec3::ZERO, offset: Vec3::ZERO }
    }

    /// Signed distance from sim-frame point `p` to the surface.
    pub fn distance(&self, p: Vec3) -> f32 {
        self.shape.distance(p - self.offset)
    }

    /// Outward unit surface normal at sim-frame point `p`.
    pub fn normal(&self, p: Vec3) -> Vec3 {
        self.shape.normal(p - self.offset)
    }
}

/// The collision world for one scene: a gravity field, a set of colliders, and the
/// dynamic bodies, advanced together on a fixed timestep.
///
/// Everything in here is **origin-relative** (ADR-0015): body positions, contact
/// points, gravity centers and ray origins are all expressed relative to `origin`,
/// a `f64` world point. Near the origin (the default), the two frames coincide.
#[derive(Default)]
pub struct PhysicsWorld {
    pub gravity: GravityField,
    pub colliders: Vec<AnchoredCollider>,
    pub bodies: Vec<Body>,
    /// Contacts resolved on the most recent `step` (cleared each step), sim frame.
    pub contacts: Vec<Contact>,
    /// World-space location of the sim's local origin. `world = origin + local`.
    pub origin: DVec3,
}

/// A raycast result: the world hit point, the surface normal there, and the distance
/// the ray travelled.
#[derive(Clone, Copy, Debug)]
pub struct RayHit {
    pub point: [f32; 3],
    pub normal: [f32; 3],
    pub distance: f32,
}

/// Sphere-trace a ray against a set of colliders (SDF terrain, triangle mesh, analytic).
/// Returns the first surface within `max_dist`, or None. The step is CAPPED so a mesh
/// collider's unsigned distance (which flattens to a large sentinel past its search reach)
/// can't make the ray overshoot — at the cost of marching in ≤1-unit steps far from any
/// surface (fine for the short rays games actually cast: ground checks, line-of-sight,
/// shots). Range is bounded by the iteration budget (~512 units).
pub fn raycast_colliders(
    colliders: &[AnchoredCollider],
    origin: Vec3,
    dir: Vec3,
    max_dist: f32,
) -> Option<RayHit> {
    let rd = dir.try_normalize()?;
    let mut t = 0.0f32;
    for _ in 0..512 {
        if t > max_dist {
            return None;
        }
        let p = origin + rd * t;
        let mut dmin = f32::MAX;
        let mut hit = 0usize;
        for (i, c) in colliders.iter().enumerate() {
            let d = c.distance(p);
            if d < dmin {
                dmin = d;
                hit = i;
            }
        }
        if !dmin.is_finite() {
            t += 1.0;
            continue;
        }
        if dmin < 0.02 {
            let n = colliders[hit].normal(p);
            return Some(RayHit { point: p.into(), normal: n.into(), distance: t });
        }
        t += dmin.clamp(0.02, 1.0); // cap so an unsigned mesh distance can't overshoot
    }
    None
}

impl PhysicsWorld {
    pub fn new(gravity: GravityField) -> Self {
        Self { gravity, ..Default::default() }
    }

    /// Add a collider whose data is in absolute world coordinates (anchor = 0).
    pub fn add_collider(&mut self, shape: Box<dyn CollisionShape>) -> usize {
        self.add_collider_at(DVec3::ZERO, shape)
    }

    /// Add a collider whose data is expressed relative to `anchor` (a world point,
    /// full `f64`). Bake geometry near ITS OWN anchor and pass the anchor here —
    /// that's what keeps collision exact for content placed far from the world origin.
    pub fn add_collider_at(&mut self, anchor: DVec3, shape: Box<dyn CollisionShape>) -> usize {
        let offset = (anchor - self.origin).as_vec3();
        self.colliders.push(AnchoredCollider { shape, anchor, offset });
        self.colliders.len() - 1
    }

    /// Recenter the sim's local frame on `new_origin` (a world point; pass a
    /// whole-number position so the shift is exact in f32). Bodies, contacts and
    /// gravity centers shift by the delta; collider offsets are recomputed from
    /// their `f64` anchors. World-space positions are unchanged — a rebase is
    /// invisible outside the sim (ADR-0015).
    pub fn rebase(&mut self, new_origin: DVec3) {
        let delta = (self.origin - new_origin).as_vec3(); // added to local positions
        if delta == Vec3::ZERO {
            return;
        }
        for b in &mut self.bodies {
            b.pos += delta;
            b.prev_pos += delta;
            b.home += delta;
        }
        for c in &mut self.contacts {
            c.point += delta;
        }
        for s in &mut self.gravity.sources {
            if let GravitySource::Point { center, .. } = s {
                *center += delta;
            }
        }
        self.origin = new_origin;
        for c in &mut self.colliders {
            c.offset = (c.anchor - new_origin).as_vec3();
        }
    }

    /// Cast a ray against every collider; the first surface hit within `max_dist`, else
    /// None. See [`raycast_colliders`].
    pub fn raycast(&self, origin: Vec3, dir: Vec3, max_dist: f32) -> Option<RayHit> {
        raycast_colliders(&self.colliders, origin, dir, max_dist)
    }

    pub fn add_body(&mut self, body: Body) -> usize {
        self.bodies.push(body);
        self.bodies.len() - 1
    }

    /// Advance the simulation by `dt` seconds. Call on a FIXED timestep (e.g. 1/120 s
    /// via an accumulator) for stability, not the variable render delta. Field-indexed
    /// throughout so the per-body collider/gravity/contact accesses stay borrow-clean.
    pub fn step(&mut self, dt: f32) {
        let dt = dt.clamp(0.0, 0.1); // guard against a huge stalled frame
        self.contacts.clear();
        for bi in 0..self.bodies.len() {
            self.step_body(bi, dt);
        }
    }

    /// Step ONE body by `dt`. Because the solver has no body-vs-body pass
    /// (bodies collide only with static colliders), a single body's step is
    /// EXACTLY the trajectory it takes inside a full [`Self::step`] — the
    /// property prediction replay depends on (`docs/netcode-design.md` §6:
    /// replay touches only the predicted body, and it's exact, not
    /// approximate). Does NOT clear `contacts`; the frame driver owns that.
    pub fn step_body(&mut self, bi: usize, dt: f32) {
        let dt = dt.clamp(0.0, 0.1);
        {
            self.bodies[bi].prev_pos = self.bodies[bi].pos; // interpolation anchor
            // Semi-implicit Euler: orient up to −gravity, integrate gravity, then move.
            // A body with `use_gravity = false` isn't pulled (and keeps its up vector).
            let g = if self.bodies[bi].use_gravity {
                self.gravity.accel_at(self.bodies[bi].pos, &self.colliders)
            } else {
                Vec3::ZERO
            };
            if g.length_squared() > 1e-10 {
                self.bodies[bi].up = (-g).normalize();
            }
            self.bodies[bi].vel += g * dt;
            let v = self.bodies[bi].vel;
            self.bodies[bi].pos += v * dt;
            self.bodies[bi].grounded = false;
            self.bodies[bi].contact = None;

            // Resolve penetration against every collider (relaxation passes), sampling
            // each of the body's collision spheres (2 for a capsule).
            for _ in 0..2 {
                for ci in 0..self.colliders.len() {
                    let (centers, n_c, radius) = self.bodies[bi].sample_centers();
                    for &c in &centers[..n_c] {
                        let pen = radius - self.colliders[ci].distance(c);
                        // `!(pen > 0.0)` also rejects NaN/Inf (a degenerate collider),
                        // so a bad distance can never push the body to a non-finite pos.
                        #[allow(clippy::neg_cmp_op_on_partial_ord)]
                        if !(pen > 0.0) {
                            continue;
                        }
                        let n = self.colliders[ci].normal(c);
                        self.bodies[bi].pos += n * pen; // push out to the surface
                        let vn = self.bodies[bi].vel.dot(n);
                        if vn < 0.0 {
                            // Reflect the normal part by restitution, damp the
                            // tangential part by friction.
                            let fr = (1.0 - self.bodies[bi].friction).clamp(0.0, 1.0);
                            let rest = self.bodies[bi].restitution;
                            let vt = self.bodies[bi].vel - n * vn;
                            self.bodies[bi].vel = vt * fr - n * vn * rest;
                        }
                        self.bodies[bi].contact = Some(n);
                        // Grounded if this contact opposes gravity (a floor, not a wall).
                        let gd = self.gravity.accel_at(self.bodies[bi].pos, &self.colliders);
                        if gd.length_squared() > 1e-6 && n.dot(-gd.normalize()) > 0.5 {
                            self.bodies[bi].grounded = true;
                        }
                        self.contacts.push(Contact { body: bi, point: c - n * radius, normal: n });
                    }
                }
            }

            // Constraints: freeze the chosen world translation axes.
            for i in 0..3 {
                if self.bodies[bi].lock_pos[i] {
                    let home = axis(self.bodies[bi].home, i);
                    set_axis(&mut self.bodies[bi].pos, i, home);
                    set_axis(&mut self.bodies[bi].vel, i, 0.0);
                }
            }
        }
    }
}

#[cfg(test)]
mod step_body_tests {
    use super::*;
    use crate::gravity::GravityField;
    use crate::shapes::Plane;

    #[test]
    fn single_body_step_matches_full_step() {
        // The prediction-replay contract: stepping body 0 alone must land it
        // bit-identically to where a full-world step puts it (no body-vs-body
        // coupling exists to break this).
        let build = || {
            let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
            w.add_collider(Box::new(Plane::ground(0.0)));
            w.add_body(Body::sphere(Vec3::new(0.0, 3.0, 0.0), 0.5));
            w.add_body(Body::sphere(Vec3::new(5.0, 3.0, 0.0), 0.5));
            w
        };
        let (mut full, mut solo) = (build(), build());
        for _ in 0..240 {
            full.step(1.0 / 120.0);
            solo.step_body(0, 1.0 / 120.0); // only body 0 advances
        }
        assert_eq!(full.bodies[0].pos, solo.bodies[0].pos, "solo step must be exact");
        assert_eq!(full.bodies[0].vel, solo.bodies[0].vel);
        assert_eq!(full.bodies[0].grounded, solo.bodies[0].grounded);
        // ...and body 1 was genuinely untouched in the solo world.
        assert_eq!(solo.bodies[1].pos, Vec3::new(5.0, 3.0, 0.0));
    }
}
