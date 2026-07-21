//! The collision world: anchored (large-world-safe) static colliders, the
//! dynamic body set, fixed-step advance, and raycasts.

use floptle_core::math::{DVec3, Quat, Vec3};

use crate::body::{axis, set_axis, Body, BodyShape, Contact};
use crate::compound::{Compound, CompoundContact};
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
    /// Collision-layer bit index (resolved from the node's named layer). Bodies
    /// only resolve against this collider when `matrix[body.layer]` has this
    /// bit set; masked raycasts skip it the same way.
    pub layer: u8,
    /// The ECS entity index of the node this collider came from (`None` for
    /// anonymous test colliders) — what collision events name as the "other".
    pub eid: Option<u32>,
    /// A trigger: the solver never pushes bodies out of it (they pass
    /// through), but overlap still reports touch events (`onTriggerEnter`…).
    pub sensor: bool,
    /// Cached `(anchor − world.origin)` as f32; queries subtract it from the probe.
    offset: Vec3,
}

impl AnchoredCollider {
    /// A collider whose data is in ABSOLUTE world coordinates (anchor = 0) — the
    /// right frame for data that's already near the world origin, and for tests.
    pub fn world(shape: Box<dyn CollisionShape>) -> Self {
        Self { shape, anchor: DVec3::ZERO, layer: 0, eid: None, sensor: false, offset: Vec3::ZERO }
    }

    /// Signed distance from sim-frame point `p` to the surface.
    pub fn distance(&self, p: Vec3) -> f32 {
        self.shape.distance(p - self.offset)
    }

    /// Outward unit surface normal at sim-frame point `p`.
    pub fn normal(&self, p: Vec3) -> Vec3 {
        self.shape.normal(p - self.offset)
    }

    /// Move the collider: a body ON RAILS (an orbiting planet) re-anchors its
    /// terrain every tick. `origin` must be the owning world's current origin.
    pub fn re_anchor(&mut self, anchor: DVec3, origin: DVec3) {
        self.anchor = anchor;
        self.offset = (anchor - origin).as_vec3();
    }
}

/// The collision world for one scene: a gravity field, a set of colliders, and the
/// dynamic bodies, advanced together on a fixed timestep.
///
/// Everything in here is **origin-relative** (ADR-0015): body positions, contact
/// points, gravity centers and ray origins are all expressed relative to `origin`,
/// a `f64` world point. Near the origin (the default), the two frames coincide.
pub struct PhysicsWorld {
    pub gravity: GravityField,
    pub colliders: Vec<AnchoredCollider>,
    pub bodies: Vec<Body>,
    /// Contacts resolved on the most recent `step` (cleared each step), sim frame.
    pub contacts: Vec<Contact>,
    /// World-space location of the sim's local origin. `world = origin + local`.
    pub origin: DVec3,
    /// The collision matrix: bit `j` of `matrix[i]` = a body on layer `i`
    /// resolves against colliders on layer `j`. Defaults to all-collide
    /// (`!0` everywhere); the sim overwrites it from the project's
    /// `floptle_core::Layers` each Play.
    pub matrix: [u32; 32],
    /// Hulls of the KINEMATIC bodies (refreshed by the sim each tick, sim
    /// frame). Dynamic bodies depenetrate from these like moving colliders —
    /// platforms/elevators push what stands on them. Only kinematic bodies
    /// appear here, and kinematic bodies skip the step, so nothing
    /// self-collides.
    pub kin_hulls: Vec<BodyHull>,
    /// Contacts a dynamic body resolved against a kinematic hull this step:
    /// `(body index, kinematic entity, point, normal)` — cleared each step,
    /// consumed by the sim's touch-event diff.
    pub kin_contacts: Vec<(usize, u32, Vec3, Vec3)>,
    /// Compound rigid bodies (multi-shape 6-DOF assemblies — see `compound.rs`),
    /// stepped alongside `bodies` with the same collider set and layer matrix.
    pub compounds: Vec<Compound>,
    /// Contacts compounds resolved on the most recent step, attributed to the
    /// shape that took them (cleared each step, sim frame).
    pub compound_contacts: Vec<CompoundContact>,
}

impl Default for PhysicsWorld {
    fn default() -> Self {
        Self {
            gravity: GravityField::default(),
            colliders: Vec::new(),
            bodies: Vec::new(),
            contacts: Vec::new(),
            origin: DVec3::ZERO,
            matrix: [!0u32; 32],
            kin_hulls: Vec::new(),
            kin_contacts: Vec::new(),
            compounds: Vec::new(),
            compound_contacts: Vec::new(),
        }
    }
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
/// `mask` filters by collision layer: bit `i` set = colliders on layer `i` are
/// testable (`!0` = everything, the no-filter default).
pub fn raycast_colliders(
    colliders: &[AnchoredCollider],
    origin: Vec3,
    dir: Vec3,
    max_dist: f32,
    mask: u32,
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
            // Sensors don't block rays either (a camera ray must pass through
            // a portal trigger exactly like the player does).
            if (mask >> c.layer) & 1 == 0 || c.sensor {
                continue;
            }
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

/// A raycastable snapshot of a dynamic body, in the sim frame. Lent to the
/// script layer alongside the colliders so rays can hit players/crates AND
/// identify which node they hit — and the thing `net.rewind` re-poses for
/// lag-compensated combat queries (`docs/netcode-design.md` §7): rewinding
/// moves these copies, never the bodies themselves.
#[derive(Clone, Copy, Debug)]
pub struct BodyHull {
    /// ECS entity index of the body's node.
    pub eid: u32,
    /// Body center, sim frame.
    pub pos: Vec3,
    pub radius: f32,
    pub shape: BodyShape,
    /// Capsule axis (kept along −gravity by the solver).
    pub up: Vec3,
    /// The body's collision-layer bit index, so masked raycasts filter dynamic
    /// bodies with the same bits as static geometry.
    pub layer: u8,
}

impl BodyHull {
    /// Signed distance from sim-frame `p` to the hull surface.
    pub fn distance(&self, p: Vec3) -> f32 {
        let d = p - self.pos;
        match self.shape {
            BodyShape::Sphere => d.length() - self.radius,
            BodyShape::Capsule { half_height } => {
                let t = d.dot(self.up).clamp(-half_height, half_height);
                (d - self.up * t).length() - self.radius
            }
            BodyShape::Box { half } => {
                let q = d.abs() - half;
                q.max(Vec3::ZERO).length() + q.max_element().min(0.0)
            }
        }
    }

    /// Outward unit normal at sim-frame `p` (central differences — rays only
    /// need it at the hit point).
    pub fn normal(&self, p: Vec3) -> Vec3 {
        const E: f32 = 1e-3;
        let n = Vec3::new(
            self.distance(p + Vec3::X * E) - self.distance(p - Vec3::X * E),
            self.distance(p + Vec3::Y * E) - self.distance(p - Vec3::Y * E),
            self.distance(p + Vec3::Z * E) - self.distance(p - Vec3::Z * E),
        );
        if n.length_squared() > 1e-12 {
            n.normalize()
        } else {
            Vec3::Y
        }
    }
}

/// Sphere-trace a ray against a set of body hulls; the first surface within
/// `max_dist` as `(entity index, hit)`, or None. `exclude` lists entities the
/// ray passes through — the caster's own body (a swing traced from a
/// character's center must not hit the character), plus any explicit ignores
/// (a camera ray skipping the character it orbits). `mask` filters by collision
/// layer, same bits as [`raycast_colliders`] (`!0` = everything). Hull
/// distances are exact analytic SDFs, so the march takes full-distance steps.
pub fn raycast_hulls(
    hulls: &[BodyHull],
    origin: Vec3,
    dir: Vec3,
    max_dist: f32,
    exclude: &[u32],
    mask: u32,
) -> Option<(u32, RayHit)> {
    let rd = dir.try_normalize()?;
    let mut t = 0.0f32;
    for _ in 0..512 {
        if t > max_dist {
            return None;
        }
        let p = origin + rd * t;
        let mut dmin = f32::MAX;
        let mut hit: Option<&BodyHull> = None;
        for h in hulls {
            if exclude.contains(&h.eid) || (mask >> h.layer) & 1 == 0 {
                continue;
            }
            let d = h.distance(p);
            if d < dmin {
                dmin = d;
                hit = Some(h);
            }
        }
        let h = hit?; // no (testable) hulls at all
        if dmin < 0.02 {
            return Some((
                h.eid,
                RayHit { point: p.into(), normal: h.normal(p).into(), distance: t },
            ));
        }
        t += dmin.max(0.02);
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
        self.add_collider_on(anchor, shape, 0)
    }

    /// [`Self::add_collider_at`], on a specific collision layer (bit index).
    pub fn add_collider_on(
        &mut self,
        anchor: DVec3,
        shape: Box<dyn CollisionShape>,
        layer: u8,
    ) -> usize {
        self.add_collider_tagged(anchor, shape, layer, None, false)
    }

    /// The full-fat registration: layer bit + source entity (what collision
    /// events name) + the sensor flag (a trigger — events without blocking).
    pub fn add_collider_tagged(
        &mut self,
        anchor: DVec3,
        shape: Box<dyn CollisionShape>,
        layer: u8,
        eid: Option<u32>,
        sensor: bool,
    ) -> usize {
        let offset = (anchor - self.origin).as_vec3();
        self.colliders.push(AnchoredCollider { shape, anchor, layer, eid, sensor, offset });
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
        for c in &mut self.compounds {
            c.pos += delta;
            c.prev_pos += delta;
        }
        for cc in &mut self.compound_contacts {
            cc.point += delta;
        }
        for c in &mut self.contacts {
            c.point += delta;
        }
        for h in &mut self.kin_hulls {
            h.pos += delta;
        }
        for (_, _, p, _) in &mut self.kin_contacts {
            *p += delta;
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
        raycast_colliders(&self.colliders, origin, dir, max_dist, !0)
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
        self.kin_contacts.clear();
        self.compound_contacts.clear();
        for bi in 0..self.bodies.len() {
            self.step_body(bi, dt);
        }
        for ci in 0..self.compounds.len() {
            self.step_compound(ci, dt);
        }
    }

    pub fn add_compound(&mut self, c: Compound) -> usize {
        self.compounds.push(c);
        self.compounds.len() - 1
    }

    /// Step ONE compound by `dt` — same solo-equals-full contract as
    /// [`Self::step_body`] (compounds couple to nothing dynamic). Does NOT
    /// clear `compound_contacts`; the frame driver owns that.
    ///
    /// Motion model: semi-implicit Euler for both linear and angular state
    /// (force/torque accumulators + gravity through the CoM), then two
    /// relaxation passes where every penetrating shape sample applies a
    /// POSITIONAL correction and a VELOCITY impulse through the generalized
    /// inverse mass `1/m + ((I⁻¹(r×n))×r)·n` — the standard rigid contact
    /// response, which is what lets an off-center contact torque the body.
    pub fn step_compound(&mut self, ci: usize, dt: f32) {
        let dt = dt.clamp(0.0, 0.1);
        if !self.compounds[ci].active {
            return;
        }
        {
            let c = &mut self.compounds[ci];
            c.prev_pos = c.pos;
            c.prev_orient = c.orient;
            let g = if c.use_gravity {
                self.gravity.accel_at(c.pos, &self.colliders)
            } else {
                Vec3::ZERO
            };
            c.vel += (g + c.force / c.mass) * dt;
            let ang_acc = c.world_inv_inertia() * c.torque;
            c.ang_vel += ang_acc * dt;
            c.force = Vec3::ZERO;
            c.torque = Vec3::ZERO;

            c.pos += c.vel * dt;
            if c.ang_vel.length_squared() > 1e-12 {
                c.orient = (Quat::from_scaled_axis(c.ang_vel * dt) * c.orient).normalize();
            }
            c.grounded = false;
        }

        let row = self.matrix[self.compounds[ci].layer as usize];
        let g_dir = {
            let g = self.gravity.accel_at(self.compounds[ci].pos, &self.colliders);
            if g.length_squared() > 1e-6 { Some(-g.normalize()) } else { None }
        };
        for _pass in 0..2 {
            // Explicit indices throughout: each resolve moves the body, so the
            // sample position is recomputed fresh for every (shape, sample,
            // collider) triple — a corrected corner must not be re-pushed from
            // its stale pre-correction position.
            for si in 0..self.compounds[ci].shapes.len() {
                let n_samples = self.compounds[ci].shape_samples(si).1;
                for k in 0..n_samples {
                    for coli in 0..self.colliders.len() {
                        if (row >> self.colliders[coli].layer) & 1 == 0
                            || self.colliders[coli].sensor
                        {
                            continue;
                        }
                        let (centers, _, radius) = self.compounds[ci].shape_samples(si);
                        let p = centers[k];
                        let pen = radius - self.colliders[coli].distance(p);
                        #[allow(clippy::neg_cmp_op_on_partial_ord)]
                        if !(pen > 0.0) {
                            continue;
                        }
                        let n = self.colliders[coli].normal(p);
                        let c = &mut self.compounds[ci];
                        let contact_pt = p - n * radius;
                        let r = contact_pt - c.pos;
                        let inv_i = c.world_inv_inertia();
                        let ang = inv_i * r.cross(n);
                        let w = 1.0 / c.mass + ang.cross(r).dot(n);
                        // Also rejects NaN (a degenerate collider normal).
                        if !(w.is_finite() && w > 1e-9) {
                            continue;
                        }
                        // Positional: push the contact point out along n. The
                        // per-resolve correction is CAPPED (translation and
                        // rotation) so a deeply-spawned assembly un-buries over
                        // a few steps instead of catapulting — uncapped, a
                        // meters-deep corner sample yields a huge λ, the
                        // rotation correction flips the body, the next sample
                        // reads even deeper, and the assembly explodes off
                        // into the sky (Ty's "cloud of scattered parts").
                        let lambda = pen / w;
                        let push = (lambda / c.mass).min(0.35);
                        c.pos += n * push;
                        let mut rot_corr = inv_i * r.cross(n * lambda);
                        let rc_len = rot_corr.length();
                        if rc_len > 0.12 {
                            rot_corr *= 0.12 / rc_len;
                        }
                        if rot_corr.length_squared() > 1e-14 {
                            c.orient = (Quat::from_scaled_axis(rot_corr) * c.orient).normalize();
                        }
                        // Velocity: normal impulse (restitution) + friction.
                        let v_p = c.vel + c.ang_vel.cross(r);
                        let vn = v_p.dot(n);
                        let mut j = 0.0;
                        if vn < 0.0 {
                            j = -(1.0 + c.restitution) * vn / w;
                            c.vel += n * (j / c.mass);
                            c.ang_vel += inv_i * r.cross(n * j);
                            // Coulomb-clamped tangential impulse.
                            let v_p = c.vel + c.ang_vel.cross(r);
                            let vt = v_p - n * v_p.dot(n);
                            let vt_len = vt.length();
                            if vt_len > 1e-6 {
                                let t = vt / vt_len;
                                let ang_t = inv_i * r.cross(t);
                                let wt = 1.0 / c.mass + ang_t.cross(r).dot(t);
                                let jt = (vt_len / wt).min(c.friction * j);
                                c.vel -= t * (jt / c.mass);
                                c.ang_vel -= inv_i * r.cross(t * jt);
                            }
                        }
                        if let Some(up) = g_dir
                            && n.dot(up) > 0.5
                        {
                            c.grounded = true;
                        }
                        let shape_id = c.shapes[si].id;
                        self.compound_contacts.push(CompoundContact {
                            compound: ci,
                            shape: si,
                            shape_id,
                            collider: coli,
                            point: contact_pt,
                            normal: n,
                            impulse: j,
                        });
                    }
                }
            }
        }

        // Rest threshold: a grounded compound whose residual motion is below
        // perceptibility comes fully to rest. Without this, corner-contact
        // micro-impulses make a parked assembly creep ~cm/s forever. Gravity
        // re-adds ~g·dt (≈0.08 at 120 Hz) each step BEFORE contacts resolve,
        // so anything genuinely sliding (a slope, ice) stays above the
        // threshold and keeps sliding — only true rest gets clamped.
        let c = &mut self.compounds[ci];
        if c.grounded && c.vel.length() < 0.05 && c.ang_vel.length() < 0.05 {
            c.vel = Vec3::ZERO;
            c.ang_vel = Vec3::ZERO;
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
        if !self.bodies[bi].active {
            return; // snapshot-driven (networked authority on a client)
        }
        if self.bodies[bi].kinematic {
            // Transform-driven: no gravity, no depenetration, no locks — the
            // sim just tracks the node (poses arrive via the kinematic sync).
            // That's the compute saving: a kinematic body costs ~nothing.
            self.bodies[bi].prev_pos = self.bodies[bi].pos;
            return;
        }
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
            // each of the body's collision spheres (2 for a capsule). The collision
            // matrix filters pairs: a body on layer i skips colliders whose layer bit
            // isn't set in matrix[i] (all-collide by default). A SENSOR body skips
            // resolution entirely — it passes through everything (overlap is detected
            // separately for the trigger hooks), so it only integrates above.
            let row = self.matrix[self.bodies[bi].layer as usize];
            let passes = if self.bodies[bi].sensor { 0 } else { 2 };
            for _ in 0..passes {
                for ci in 0..self.colliders.len() {
                    if (row >> self.colliders[ci].layer) & 1 == 0 {
                        continue;
                    }
                    // Sensors never block — overlap is detected separately
                    // (touch events), the body passes straight through.
                    if self.colliders[ci].sensor {
                        continue;
                    }
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
                        self.contacts.push(Contact {
                            body: bi,
                            collider: ci,
                            point: c - n * radius,
                            normal: n,
                        });
                    }
                }
                // …and against the KINEMATIC bodies' hulls — moving platforms
                // and elevators push dynamic bodies exactly like static
                // geometry would (only kinematic bodies live in `kin_hulls`,
                // and kinematic bodies skip the step, so nothing self-hits).
                for hi in 0..self.kin_hulls.len() {
                    let hull = self.kin_hulls[hi];
                    if (row >> hull.layer) & 1 == 0 {
                        continue;
                    }
                    let (centers, n_c, radius) = self.bodies[bi].sample_centers();
                    for &c in &centers[..n_c] {
                        let pen = radius - hull.distance(c);
                        #[allow(clippy::neg_cmp_op_on_partial_ord)]
                        if !(pen > 0.0) {
                            continue;
                        }
                        let n = hull.normal(c);
                        self.bodies[bi].pos += n * pen;
                        let vn = self.bodies[bi].vel.dot(n);
                        if vn < 0.0 {
                            let fr = (1.0 - self.bodies[bi].friction).clamp(0.0, 1.0);
                            let rest = self.bodies[bi].restitution;
                            let vt = self.bodies[bi].vel - n * vn;
                            self.bodies[bi].vel = vt * fr - n * vn * rest;
                        }
                        self.bodies[bi].contact = Some(n);
                        let gd = self.gravity.accel_at(self.bodies[bi].pos, &self.colliders);
                        if gd.length_squared() > 1e-6 && n.dot(-gd.normalize()) > 0.5 {
                            self.bodies[bi].grounded = true;
                        }
                        self.kin_contacts.push((bi, hull.eid, c - n * radius, n));
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

#[cfg(test)]
mod hull_tests {
    use super::*;

    fn capsule_at(eid: u32, x: f32) -> BodyHull {
        BodyHull {
            eid,
            pos: Vec3::new(x, 1.0, 0.0),
            radius: 0.4,
            shape: BodyShape::Capsule { half_height: 0.6 },
            up: Vec3::Y,
            layer: 0,
        }
    }

    #[test]
    fn ray_hits_the_nearest_hull_and_identifies_it() {
        let hulls = [capsule_at(7, 5.0), capsule_at(9, 10.0)];
        let (eid, hit) =
            raycast_hulls(&hulls, Vec3::new(0.0, 1.0, 0.0), Vec3::X, 50.0, &[], !0).expect("hit");
        assert_eq!(eid, 7, "the nearer capsule wins");
        assert!((hit.distance - 4.6).abs() < 0.05, "surface at x = 5 − 0.4, got {}", hit.distance);
        assert!(hit.normal[0] < -0.9, "normal faces the ray");
    }

    #[test]
    fn exclusion_skips_the_caster_own_body() {
        // The ray STARTS INSIDE hull 7 (a swing from the character's center).
        let hulls = [capsule_at(7, 0.0), capsule_at(9, 10.0)];
        let (eid, _) = raycast_hulls(&hulls, Vec3::new(0.0, 1.0, 0.0), Vec3::X, 50.0, &[7], !0)
            .expect("must hit the other hull");
        assert_eq!(eid, 9);
        // Without exclusion it self-hits immediately.
        let (eid, hit) =
            raycast_hulls(&hulls, Vec3::new(0.0, 1.0, 0.0), Vec3::X, 50.0, &[], !0).unwrap();
        assert_eq!(eid, 7);
        assert_eq!(hit.distance, 0.0);
    }

    #[test]
    fn layer_mask_filters_hulls_like_exclusion() {
        // Hull 7 sits on layer 2; a mask without bit 2 rays straight through it.
        let mut near = capsule_at(7, 5.0);
        near.layer = 2;
        let hulls = [near, capsule_at(9, 10.0)];
        let (eid, _) =
            raycast_hulls(&hulls, Vec3::new(0.0, 1.0, 0.0), Vec3::X, 50.0, &[], !(1 << 2))
                .expect("must hit the layer-0 hull behind");
        assert_eq!(eid, 9);
        // With the bit set, the nearer hull wins again.
        let (eid, _) =
            raycast_hulls(&hulls, Vec3::new(0.0, 1.0, 0.0), Vec3::X, 50.0, &[], !0).unwrap();
        assert_eq!(eid, 7);
    }

    #[test]
    fn capsule_side_and_cap_distances() {
        let h = capsule_at(1, 0.0);
        // Side: radial distance minus radius.
        assert!((h.distance(Vec3::new(2.0, 1.0, 0.0)) - 1.6).abs() < 1e-5);
        // Above the top cap: center + half_height + radius = y 2.0.
        assert!(h.distance(Vec3::new(0.0, 2.0, 0.0)).abs() < 1e-5);
        // Inside is negative.
        assert!(h.distance(Vec3::new(0.0, 1.0, 0.0)) < 0.0);
    }

    #[test]
    fn ray_misses_within_range_returns_none() {
        let hulls = [capsule_at(1, 5.0)];
        assert!(raycast_hulls(&hulls, Vec3::ZERO, Vec3::Y, 100.0, &[], !0).is_none());
        assert!(raycast_hulls(&hulls, Vec3::new(0.0, 1.0, 0.0), Vec3::X, 2.0, &[], !0).is_none());
    }
}
