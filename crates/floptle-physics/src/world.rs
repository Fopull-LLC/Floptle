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

    /// The collider's bounding sphere in the SIM frame, if its shape has one
    /// (`floptle/0076`). `None` = no useful bound, so the broadphase always
    /// offers it and the narrow phase decides, exactly as before.
    pub fn bounds(&self) -> Option<(Vec3, f32)> {
        self.shape.bounds().map(|(c, r)| (c + self.offset, r))
    }

    /// Signed distance from sim-frame point `p` to the surface.
    pub fn distance(&self, p: Vec3) -> f32 {
        self.shape.distance(p - self.offset)
    }

    /// Outward unit surface normal at sim-frame point `p`.
    pub fn normal(&self, p: Vec3) -> Vec3 {
        self.shape.normal(p - self.offset)
    }

    /// The outward normal only where the field resolves one (`None` deep inside
    /// a saturated SDF interior — see [`CollisionShape::normal_reliable`]).
    pub fn normal_reliable(&self, p: Vec3) -> Option<Vec3> {
        self.shape.normal_reliable(p - self.offset)
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
    /// The scene's bodies of water (`floptle/0038`). Rebuilt from the scene's
    /// WaterVolume nodes every frame, exactly like `gravity` (`floptle/0141`) —
    /// **static only *within* one step**, which is what keeps
    /// `Sim::step_body_tick` bit-for-bit exact. That is a claim about the field
    /// not moving *while a tick is running*, not about being built once per
    /// session: a pool spawned, moved, resized or destroyed while the game is
    /// running is in this field the same frame it is in the renderer's gather.
    pub water: crate::water::WaterField,
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
    /// Broadphase over the colliders, rebuilt at the top of every `step`
    /// (`floptle/0076`).
    ///
    /// Rebuilt rather than cached-and-invalidated on purpose: `colliders` is a
    /// public Vec that the sim rewrites wholesale, terrain edits mutate in place,
    /// and `rebase` moves every offset at once. A per-step rebuild is O(colliders)
    /// against a pass that is bodies x colliders x 2, so it pays for itself at two
    /// bodies — and it cannot go stale, which no invalidation scheme can promise.
    collider_index: floptle_core::spatial::Grid,
    /// Scratch candidate list, reused so the broadphase allocates nothing per body.
    cand: Vec<u32>,
    /// True while `step` has already rebuilt the index for this tick.
    ///
    /// `step_body` is also reachable directly (the rollback driver steps bodies
    /// one at a time), and an index left over from another collider set would
    /// drop contacts — a body falling through the floor. So `step_body` rebuilds
    /// unless `step` just did, which costs the driven path what it cost before
    /// and never risks a stale answer.
    index_fresh: bool,
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
            water: crate::water::WaterField::default(),
            colliders: Vec::new(),
            bodies: Vec::new(),
            contacts: Vec::new(),
            origin: DVec3::ZERO,
            matrix: [!0u32; 32],
            kin_hulls: Vec::new(),
            kin_contacts: Vec::new(),
            compounds: Vec::new(),
            compound_contacts: Vec::new(),
            collider_index: Default::default(),
            cand: Vec::new(),
            index_fresh: false,
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

/// One thing a shape query found.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShapeHit {
    /// The body or collider's ECS entity index (`None` for anonymous colliders).
    pub eid: Option<u32>,
    /// The closest point on the queried shape's surface to the thing it found.
    pub point: [f32; 3],
    /// Outward normal at `point`, pointing away from what was hit.
    pub normal: [f32; 3],
    /// Overlap depth for an overlap query; travel distance for a cast.
    pub distance: f32,
}

/// Everything a sphere of `radius` at `center` intersects.
///
/// Cheap by construction and exactly in this engine's idiom: every collider
/// already answers a signed distance, so "does this overlap" is
/// `distance(center) < radius` — no separate broadphase, no new geometry
/// kernels. Sensors are included (a hitbox wants to know it swept a trigger
/// volume); the ray queries skip them because a camera ray must pass through a
/// portal trigger, which is a different question.
pub fn overlap_sphere_colliders(
    colliders: &[AnchoredCollider],
    center: Vec3,
    radius: f32,
    mask: u32,
) -> Vec<ShapeHit> {
    let mut out = Vec::new();
    for c in colliders {
        if (mask >> c.layer) & 1 == 0 {
            continue;
        }
        let d = c.distance(center);
        if d < radius {
            let n = c.normal(center);
            out.push(ShapeHit {
                eid: c.eid,
                point: (center - n * d).into(),
                normal: n.into(),
                distance: radius - d,
            });
        }
    }
    out
}

/// Everything a sphere of `radius` at `center` intersects among BODY hulls.
///
/// This is the half a melee hitbox cares about, and the half lag compensation
/// moves: inside `net.rewind` the hulls handed in are the rewound ones, so an
/// overlap sees the world as the attacker saw it — the promise the netcode
/// design made for shape queries and only raycast had kept.
pub fn overlap_sphere_hulls(
    hulls: &[BodyHull],
    center: Vec3,
    radius: f32,
    exclude: &[u32],
    mask: u32,
) -> Vec<ShapeHit> {
    let mut out = Vec::new();
    for h in hulls {
        if exclude.contains(&h.eid) || (mask >> h.layer) & 1 == 0 {
            continue;
        }
        let d = h.distance(center);
        if d < radius {
            let n = h.normal(center);
            out.push(ShapeHit {
                eid: Some(h.eid),
                point: (center - n * d).into(),
                normal: n.into(),
                distance: radius - d,
            });
        }
    }
    out
}

/// Sweep a sphere of `radius` along a ray; the first thing it touches.
///
/// The same march `raycast_colliders` runs, with the radius subtracted from
/// every distance — which is what makes a swept sphere free here: an SDF's
/// `d - r` IS the sphere's distance field. `exclude` and `mask` behave exactly
/// as they do for a ray, so `{layers = …}` means the same thing for both.
#[allow(clippy::too_many_arguments)]
pub fn spherecast(
    colliders: &[AnchoredCollider],
    hulls: &[BodyHull],
    origin: Vec3,
    dir: Vec3,
    radius: f32,
    max_dist: f32,
    exclude: &[u32],
    mask: u32,
) -> Option<ShapeHit> {
    let rd = dir.try_normalize()?;
    let r = radius.max(0.0);
    let mut t = 0.0f32;
    for _ in 0..512 {
        if t > max_dist {
            return None;
        }
        let p = origin + rd * t;
        let mut dmin = f32::MAX;
        let mut best: Option<(Option<u32>, Vec3)> = None;
        for c in colliders {
            if (mask >> c.layer) & 1 == 0 || c.sensor {
                continue;
            }
            let d = c.distance(p) - r;
            if d < dmin {
                dmin = d;
                best = Some((c.eid, c.normal(p)));
            }
        }
        for h in hulls {
            if exclude.contains(&h.eid) || (mask >> h.layer) & 1 == 0 {
                continue;
            }
            let d = h.distance(p) - r;
            if d < dmin {
                dmin = d;
                best = Some((Some(h.eid), h.normal(p)));
            }
        }
        let (eid, n) = best?; // nothing testable in range at all
        if dmin < 0.02 {
            // The contact is on the swept sphere's surface, not its centre.
            return Some(ShapeHit {
                eid,
                point: (p - n * r).into(),
                normal: n.into(),
                distance: t,
            });
        }
        // Same floor as the ray march: an SDF that reports a huge sentinel far
        // from any surface must not let the sweep step over thin geometry.
        t += dmin.clamp(0.02, 1.0);
    }
    None
}

/// Sweep a vertical capsule (the shape a character actually is) along a ray.
///
/// Approximated as spheres at the two cap centres, which is exact for the caps
/// and conservative along the barrel — and matches how the solver treats a
/// capsule body, so a cast agrees with the movement it is predicting.
#[allow(clippy::too_many_arguments)]
pub fn capsulecast(
    colliders: &[AnchoredCollider],
    hulls: &[BodyHull],
    origin: Vec3,
    dir: Vec3,
    radius: f32,
    half_height: f32,
    up: Vec3,
    max_dist: f32,
    exclude: &[u32],
    mask: u32,
) -> Option<ShapeHit> {
    let u = up.try_normalize().unwrap_or(Vec3::Y);
    let h = (half_height - radius).max(0.0);
    let a = spherecast(colliders, hulls, origin + u * h, dir, radius, max_dist, exclude, mask);
    let b = spherecast(colliders, hulls, origin - u * h, dir, radius, max_dist, exclude, mask);
    match (a, b) {
        (Some(a), Some(b)) => Some(if a.distance <= b.distance { a } else { b }),
        (Some(a), None) => Some(a),
        (None, b) => b,
    }
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
    /// Rebuild the collider broadphase from the current collider set
    /// (`floptle/0076`).
    pub(crate) fn reindex_colliders(&mut self) {
        // A collider with no bound (a plane, a terrain field, a mesh) is handed
        // in with an infinite radius, which the grid files as oversized and
        // therefore offers to every query. That is what makes this a pure
        // narrowing: nothing the scan tested can stop being tested.
        let items = self.colliders.iter().map(|c| match c.bounds() {
            Some((centre, r)) => (centre, r),
            None => (Vec3::ZERO, f32::INFINITY),
        });
        self.collider_index.rebuild(items);
        self.index_fresh = true;
    }

    pub fn step(&mut self, dt: f32) {
        let dt = dt.clamp(0.0, 0.1); // guard against a huge stalled frame
        self.reindex_colliders();
        self.contacts.clear();
        self.kin_contacts.clear();
        self.compound_contacts.clear();
        for bi in 0..self.bodies.len() {
            // Driver-stepped bodies advance through `Sim::step_body_tick`
            // instead — see `Body::driven`.
            if !self.bodies[bi].driven {
                self.step_body(bi, dt);
            }
        }
        for ci in 0..self.compounds.len() {
            self.step_compound(ci, dt);
        }
        self.index_fresh = false;
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
    /// Buoyancy + drag on one compound, shape by shape.
    ///
    /// Each shape's push is applied at the shape's own world position, so the
    /// lever arm about the centre of mass is real and a lopsided hull rights
    /// itself for free. Angular drag is applied once, to the whole body,
    /// scaled by how much of it is wet — a craft half out of the water should
    /// not be damped like a submarine.
    fn apply_water_to_compound(&mut self, ci: usize, dt: f32) {
        let (com, orient, mass, n) = {
            let c = &self.compounds[ci];
            (c.pos, c.orient, c.mass, c.shapes.len())
        };
        if n == 0 {
            return;
        }
        let g = self.gravity.accel_at(com, &self.colliders);
        let mut wet_shapes = 0usize;
        for si in 0..n {
            let (offset, radius, share) = {
                let s = &self.compounds[ci].shapes[si];
                let r = match s.geom {
                    crate::ShapeGeom::Sphere { radius } => radius,
                    crate::ShapeGeom::Capsule { radius, half_height } => radius + half_height,
                    crate::ShapeGeom::Box { half } => half.length(),
                };
                // The shape's share of the body's mass decides whether IT
                // floats — a heavy engine block low in a hull should pull that
                // end down even when the light nose is buoyant.
                (s.offset, r, s.mass.max(1e-4))
            };
            let p = com + orient * offset;
            // The shape's own velocity, not the body's: a spinning craft's
            // submerged end is moving through the water even when the centre
            // of mass is not, and that is what damps the spin.
            let c = &self.compounds[ci];
            let v = c.vel + c.ang_vel.cross(p - com);
            let Some(a) = crate::water::buoyancy_accel(&self.water, p, radius, share, v, g, dt)
            else {
                continue;
            };
            wet_shapes += 1;
            // `a` is per-shape-mass; turn it back into a force so the whole
            // body's response is the sum, applied at the shape's position.
            let force = a * share;
            let c = &mut self.compounds[ci];
            c.vel += force / mass * dt;
            let torque = (p - com).cross(force);
            let ang_acc = c.world_inv_inertia() * torque;
            c.ang_vel += ang_acc * dt;
        }
        if wet_shapes == 0 {
            return;
        }
        // Angular drag, once, proportional to how much of the craft is under.
        // Exponential rather than subtractive so it can never flip the spin.
        let wet = wet_shapes as f32 / n as f32;
        let k = self
            .water
            .volumes
            .iter()
            .find(|v| !v.frozen)
            .map(|v| v.angular_drag)
            .unwrap_or(0.0);
        if k > 0.0 {
            let c = &mut self.compounds[ci];
            c.ang_vel *= 1.0 / (1.0 + k * wet * dt);
        }
    }

    pub fn step_compound(&mut self, ci: usize, dt: f32) {
        let dt = dt.clamp(0.0, 0.1);
        if !self.compounds[ci].active {
            return;
        }
        if self.compounds[ci].anchored {
            // Pinned in place (launch clamps, docking latches): the pose holds,
            // accumulated forces drop, and velocities stay zero so release
            // resumes from rest — no impulse builds up while clamped.
            let c = &mut self.compounds[ci];
            c.prev_pos = c.pos;
            c.prev_orient = c.orient;
            c.vel = Vec3::ZERO;
            c.ang_vel = Vec3::ZERO;
            c.force = Vec3::ZERO;
            c.torque = Vec3::ZERO;
            c.grounded = true;
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
        }
        // WATER, per SHAPE. A hull that lands flat floats; the same hull
        // nose-down sinks its nose and rights itself — and that difference is
        // entirely about WHERE the displaced volume is, so each shape's push is
        // applied at its own position and the inertia tensor turns the
        // asymmetry into torque. One force at the centre of mass gives a craft
        // that bobs but never rights itself, which reads as a trampoline.
        if !self.water.is_empty() {
            self.apply_water_to_compound(ci, dt);
        }
        {
            let c = &mut self.compounds[ci];

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
        // Per-STEP correction budget across ALL of this compound's contacts:
        // the per-resolve caps bound each sample, but a many-part assembly
        // deep inside geometry touches dozens of samples × two passes — the
        // sum ejected buried vessels at astronautical speed. Once the budget
        // is spent, remaining penetrations wait for the next step (an
        // un-burying assembly climbs out at a bounded, sane rate).
        let mut push_budget = 0.35f32;
        let mut rot_budget = 0.12f32;
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
                        // A reliable surface normal where the field resolves one;
                        // deep in a saturated SDF interior (a fast ram that
                        // tunnelled past the narrow band) the gradient is zero, so
                        // fall back to the body's OWN travel direction at this
                        // point. That makes the contact report the true closing
                        // speed (and shove it back the way it came) instead of the
                        // bogus straight-up normal that read a lithobrake as ~0 m/s.
                        let n = match self.colliders[coli].normal_reliable(p) {
                            Some(n) => n,
                            None => {
                                let cc = &self.compounds[ci];
                                let vp = cc.vel + cc.ang_vel.cross(p - cc.pos);
                                (-vp).try_normalize().unwrap_or(Vec3::Y)
                            }
                        };
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
                        let push = (lambda / c.mass).min(push_budget);
                        push_budget -= push;
                        c.pos += n * push;
                        let mut rot_corr = inv_i * r.cross(n * lambda);
                        let rc_len = rot_corr.length();
                        if rc_len > rot_budget {
                            rot_corr *= rot_budget / rc_len.max(1e-9);
                        }
                        rot_budget -= rot_corr.length();
                        if rot_corr.length_squared() > 1e-14 {
                            c.orient = (Quat::from_scaled_axis(rot_corr) * c.orient).normalize();
                        }
                        // Velocity: normal impulse (restitution) + friction.
                        let v_p = c.vel + c.ang_vel.cross(r);
                        let vn = v_p.dot(n);
                        // Incoming closing speed, captured BEFORE the impulse is
                        // applied (negative vn = approaching). Not subject to the
                        // depenetration budget, so it reports true crash speed.
                        let speed = (-vn).max(0.0);
                        // TOTAL contact-point speed (normal + tangential): the
                        // energy the hit actually carries, independent of how the
                        // surface happens to be angled. A crash model wants this,
                        // not the normal component that a curved planet guts.
                        let speed_abs = v_p.length();
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
                            speed,
                            speed_abs,
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

    /// Record one resolved contact on a body: the telegraph normal, whether it
    /// counts as ground, and the step's two extremes — the most floor-like
    /// surface (`ground_normal`) and the steepest one (`wall_normal`).
    ///
    /// Extremes rather than "the last one seen", because a character standing
    /// at the foot of a cliff touches BOTH, and which arrived last is just
    /// collider order. Both use the same 0.5 (60°) cut that decides `grounded`,
    /// so "grounded" and "has a ground normal" are one fact, and a surface is
    /// never reported as both floor and wall.
    fn note_body_contact(&mut self, bi: usize, n: Vec3) {
        self.bodies[bi].contact = Some(n);
        let gd = self.gravity.accel_at(self.bodies[bi].pos, &self.colliders);
        if gd.length_squared() <= 1e-6 {
            return;
        }
        let up = -gd.normalize();
        let d = n.dot(up);
        // The body's own slope limit decides what counts as ground (60° by
        // default, which is the constant this used to be).
        if d > self.bodies[bi].slope_limit.clamp(0.0, std::f32::consts::FRAC_PI_2).cos() {
            self.bodies[bi].grounded = true;
            if self.bodies[bi].ground_normal.is_none_or(|g| g.dot(up) < d) {
                self.bodies[bi].ground_normal = Some(n);
            }
        } else if self.bodies[bi].wall_normal.is_none_or(|w| w.dot(up) > d) {
            self.bodies[bi].wall_normal = Some(n);
        }
    }

    /// Spend a **Coulomb friction budget** on body `bi`, sideways along the
    /// surface `n`.
    ///
    /// `budget` is the load the surface is carrying this step, expressed as a
    /// speed (an impulse per unit mass): the weight it holds up, or the impact
    /// it just absorbed. Friction can remove at most `friction × budget` of
    /// tangential speed — never more than there is — which is what makes "does
    /// this ramp hold" a question with an answer (`tan(angle) ≤ friction`)
    /// instead of a race between gravity and a damping factor.
    ///
    /// It opposes motion; it does not clamp it. A body shoved across a floor
    /// travels and then stops, rather than stopping in three steps because a
    /// multiplier ate its velocity.
    fn rub(&mut self, bi: usize, budget: f32, n: Vec3) {
        // No upper clamp: a coefficient above 1 is an ordinary grippy surface
        // (rubber on rubber is about 1.5), and it is the only way to say "you
        // can stand on this 50° ramp" — 1.0 holds exactly 45°.
        let mu = self.bodies[bi].friction.max(0.0);
        #[allow(clippy::neg_cmp_op_on_partial_ord)]
        if mu <= 0.0 || !(budget > 0.0) {
            return;
        }
        let vel = self.bodies[bi].vel;
        let vn = vel.dot(n);
        let vt = vel - n * vn;
        let len = vt.length();
        if len <= 1e-6 {
            return;
        }
        let dv = (mu * budget).min(len);
        self.bodies[bi].vel = vt * ((len - dv) / len) + n * vn;
    }

    /// Step ONE body by `dt`. Because the solver has no body-vs-body pass
    /// (bodies collide only with static colliders), a single body's step is
    /// EXACTLY the trajectory it takes inside a full [`Self::step`] — the
    /// property prediction replay depends on (`docs/netcode-design.md` §6:
    /// replay touches only the predicted body, and it's exact, not
    /// approximate). Does NOT clear `contacts`; the frame driver owns that.
    pub fn step_body(&mut self, bi: usize, dt: f32) {
        let dt = dt.clamp(0.0, 0.1);
        if !self.index_fresh {
            // Reached directly (the rollback driver), so nothing has built the
            // broadphase for this tick yet.
            self.reindex_colliders();
            self.index_fresh = false; // one body only; the next one rebuilds too
        }
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
        if self.bodies[bi].pushbox_only {
            // Integrate the velocity, and stop. Everything below this line —
            // gravity, the depenetration relaxation, ground detection, the
            // position locks — is exactly the machinery a rollback session
            // cannot rely on to agree in the last bit on two different
            // machines, and exactly the machinery a fighting game replaces with
            // integer frame data anyway (`docs/rollback-netcode-design.md` §3).
            //
            // Not `kinematic`: this body still MOVES under its own velocity, so
            // the controller's whole existing velocity channel keeps working.
            // It just moves without being negotiated with.
            let b = &mut self.bodies[bi];
            b.prev_pos = b.pos;
            b.pos += b.vel * dt;
            b.contact = None;
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
            // WATER. Applied as an acceleration alongside gravity, from the
            // same static field, so a body in a sea is still one body against
            // the world — nothing here reads another body, and the rollback
            // contract holds. A single-shape body gets one sample at its
            // centre; the per-shape treatment that lets a hull right itself is
            // the compound path's, because a capsule has no orientation worth
            // torquing.
            if !self.water.is_empty() {
                let b = &self.bodies[bi];
                let (_, _, r) = b.sample_centers();
                // A box body has no sphere radius; use its bounding sphere so
                // a crate still displaces something.
                let r = if r > 0.0 { r } else { b.bounding_radius() };
                if let Some(a) = crate::water::buoyancy_accel(
                    &self.water,
                    b.pos,
                    r,
                    b.mass,
                    b.vel,
                    g,
                    dt,
                ) {
                    self.bodies[bi].vel += a * dt;
                }
            }
            // ---- friction, part one: the weight the floor is holding up -----
            //
            // BEFORE the move, against the floor found last step. A body parked
            // on a ramp gains `g_t · dt` of downhill speed from the gravity line
            // above; removing it here means the body never travels that
            // distance. Doing it after the move instead leaves `g_t · dt²` of
            // creep per step — a few centimetres a minute, which is precisely
            // the "everything slides downhill eventually" complaint, and it
            // survives any amount of friction because it is a position error,
            // not a velocity one.
            if let Some(n) = self.bodies[bi].ground_normal {
                self.rub(bi, (-g.dot(n)).max(0.0) * dt, n);
            }
            let v = self.bodies[bi].vel;
            self.bodies[bi].pos += v * dt;
            self.bodies[bi].grounded = false;
            self.bodies[bi].contact = None;
            self.bodies[bi].ground_normal = None;
            self.bodies[bi].wall_normal = None;

            // Resolve penetration against every collider (relaxation passes), sampling
            // each of the body's collision spheres (2 for a capsule). The collision
            // matrix filters pairs: a body on layer i skips colliders whose layer bit
            // isn't set in matrix[i] (all-collide by default). A SENSOR body skips
            // resolution entirely — it passes through everything (overlap is detected
            // separately for the trigger hooks), so it only integrates above.
            let row = self.matrix[self.bodies[bi].layer as usize];
            let passes = if self.bodies[bi].sensor { 0 } else { 2 };
            // The normal impulse this step, as a speed (per unit mass): how much
            // into-surface velocity the contacts had to remove. Friction is
            // Coulomb — bounded by the load the surface is actually carrying —
            // so this is what a hard landing has that a gentle touchdown does
            // not, and it is why you skid when you land fast and stick when you
            // don't. Accumulated here and spent ONCE below.
            let mut impact_dv = 0.0f32;
            for _ in 0..passes {
                // Broadphase (`floptle/0076`): ask the index which colliders can
                // possibly reach this body, instead of walking all of them. The
                // query sphere covers every sample centre plus the body radius,
                // and a collider outside it cannot produce `radius - d > 0` — so
                // this narrows what is TESTED and cannot change what is FOUND.
                //
                // Re-queried each pass because depenetration moves the body, and
                // in ASCENDING index order — the same order the full scan visited
                // them, which is what keeps a rollback re-simulation bit-exact.
                let cand = {
                    let (centres, n_c, radius) = self.bodies[bi].sample_centers();
                    let pos = self.bodies[bi].pos;
                    let reach = centres[..n_c]
                        .iter()
                        .map(|c| c.distance(pos))
                        .fold(0.0f32, f32::max);
                    let mut cand = std::mem::take(&mut self.cand);
                    cand.clear();
                    self.collider_index.sphere(pos, reach + radius + 0.01, &mut cand);
                    cand
                };
                for &ci in &cand {
                    let ci = ci as usize;
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
                            // Reflect the normal part by restitution and bank
                            // the load; the tangential part is left alone here
                            // and answered once, after every contact is in.
                            let rest = self.bodies[bi].restitution;
                            let vt = self.bodies[bi].vel - n * vn;
                            self.bodies[bi].vel = vt - n * vn * rest;
                            impact_dv += -vn * (1.0 + rest);
                        }
                        self.note_body_contact(bi, n);
                        self.contacts.push(Contact {
                            body: bi,
                            collider: ci,
                            point: c - n * radius,
                            normal: n,
                        });
                    }
                }
                // Hand the buffer back so the next pass (and the next body)
                // reuses its allocation.
                self.cand = cand;
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
                            let rest = self.bodies[bi].restitution;
                            let vt = self.bodies[bi].vel - n * vn;
                            self.bodies[bi].vel = vt - n * vn * rest;
                            impact_dv += -vn * (1.0 + rest);
                        }
                        // A moving platform is ground exactly like static
                        // geometry is, so it goes through the same classifier —
                        // the body's slope limit, its floor and wall normals,
                        // and therefore its friction.
                        self.note_body_contact(bi, n);
                        self.kin_contacts.push((bi, hull.eid, c - n * radius, n));
                    }
                }
            }

            // ---- friction, part two: what the contacts just absorbed --------
            //
            // Spent ONCE, on the surface actually underfoot, however many
            // colliders the body turned out to be touching. This is the part
            // that makes a fast landing skid and a gentle one stick, and the
            // reason a wall does not slow a body sliding down it: a wall you are
            // not pushing into absorbs nothing, so there is nothing to spend.
            let surface = self.bodies[bi].ground_normal.or(self.bodies[bi].contact);
            if let Some(n) = surface {
                // Minus the weight, which part one already spent: a body resting
                // on a slope pushes into it by `g·n · dt` every step and the
                // contact dutifully cancels that, so counting it here too would
                // charge the same load twice and make every surface exactly
                // twice as grippy as its number says.
                let already = (-g.dot(n)).max(0.0) * dt;
                self.rub(bi, impact_dv - already, n);
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

#[cfg(test)]
mod shape_query_tests {
    use super::*;
    use crate::BodyShape;

    fn hull(eid: u32, at: Vec3, radius: f32) -> BodyHull {
        BodyHull { eid, pos: at, radius, shape: BodyShape::Sphere, up: Vec3::Y, layer: 0 }
    }

    /// The question `raycast` could not answer: what is INSIDE this volume. A
    /// fan of rays misses anything thinner than the fan and cannot report depth.
    #[test]
    fn overlap_sphere_finds_every_body_inside_it_and_nothing_outside() {
        let hulls = [
            hull(1, Vec3::new(0.0, 0.0, 0.0), 0.5), // dead centre
            hull(2, Vec3::new(1.8, 0.0, 0.0), 0.5), // just inside the rim
            hull(3, Vec3::new(6.0, 0.0, 0.0), 0.5), // well outside
        ];
        let hits = overlap_sphere_hulls(&hulls, Vec3::ZERO, 2.0, &[], !0);
        let mut ids: Vec<u32> = hits.iter().filter_map(|h| h.eid).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2]);
        // Depth is reported, and the centred body is deeper than the rim one.
        let d1 = hits.iter().find(|h| h.eid == Some(1)).unwrap().distance;
        let d2 = hits.iter().find(|h| h.eid == Some(2)).unwrap().distance;
        assert!(d1 > d2, "the body at the centre overlaps more deeply ({d1} vs {d2})");
    }

    /// `exclude` and the layer mask mean exactly what they do for a ray, because
    /// a hitbox centred on you must not report you.
    #[test]
    fn overlap_honours_exclude_and_the_layer_mask() {
        let mut a = hull(1, Vec3::ZERO, 0.5);
        a.layer = 2;
        let b = hull(2, Vec3::new(0.5, 0.0, 0.0), 0.5); // layer 0
        let hulls = [a, b];
        assert_eq!(overlap_sphere_hulls(&hulls, Vec3::ZERO, 2.0, &[1], !0).len(), 1);
        // Only layer 2.
        let only2 = overlap_sphere_hulls(&hulls, Vec3::ZERO, 2.0, &[], 1 << 2);
        assert_eq!(only2.len(), 1);
        assert_eq!(only2[0].eid, Some(1));
    }

    /// **The lag-compensation property.** The design promised rewound overlaps
    /// and only `raycast` kept it. These take the hull set as an argument, so
    /// inside `net.rewind` — which hands over rewound hulls — an overlap sees
    /// the world as the attacker saw it. Same query, two worlds, two answers.
    #[test]
    fn an_overlap_sees_whatever_world_it_is_handed() {
        let live = [hull(7, Vec3::new(5.0, 0.0, 0.0), 0.5)]; // has since run away
        let rewound = [hull(7, Vec3::new(0.4, 0.0, 0.0), 0.5)]; // where it was
        let swing = Vec3::ZERO;
        assert!(
            overlap_sphere_hulls(&live, swing, 1.5, &[], !0).is_empty(),
            "against the live world the swing misses"
        );
        assert_eq!(
            overlap_sphere_hulls(&rewound, swing, 1.5, &[], !0)
                .first()
                .and_then(|h| h.eid),
            Some(7),
            "against the rewound world it connects — which is what the attacker saw"
        );
    }

    /// A swept sphere hits things a ray down the same line passes beside — the
    /// whole reason a thrown object is not a ray.
    #[test]
    fn a_swept_sphere_catches_what_a_ray_squeaks_past() {
        // Offset from the ray line by more than the body's radius…
        let hulls = [hull(1, Vec3::new(5.0, 0.9, 0.0), 0.5)];
        let (o, d) = (Vec3::ZERO, Vec3::X);
        assert!(
            raycast_hulls(&hulls, o, d, 20.0, &[], !0).is_none(),
            "a bare ray misses it"
        );
        let hit = spherecast(&[], &hulls, o, d, 0.6, 20.0, &[], !0)
            .expect("a sphere of radius 0.6 does not");
        assert_eq!(hit.eid, Some(1));
        assert!(hit.distance > 0.0 && hit.distance < 6.0, "and stops at it: {}", hit.distance);
    }

    /// A cast that starts pointing at nothing reports nothing, rather than
    /// marching to the horizon and inventing a hit.
    #[test]
    fn a_cast_into_empty_space_misses() {
        let hulls = [hull(1, Vec3::new(0.0, 0.0, 40.0), 0.5)];
        assert!(spherecast(&[], &hulls, Vec3::ZERO, Vec3::X, 0.5, 10.0, &[], !0).is_none());
        assert!(spherecast(&[], &[], Vec3::ZERO, Vec3::X, 0.5, 10.0, &[], !0).is_none());
    }

    /// A capsule sweep catches what its ENDS meet, not just its middle — the
    /// difference between "can I walk there" and "is my navel clear".
    #[test]
    fn a_capsule_sweep_catches_what_only_its_head_meets() {
        // A body at head height only; a sphere cast from the centre misses it.
        let hulls = [hull(1, Vec3::new(4.0, 1.6, 0.0), 0.5)];
        let (o, d) = (Vec3::ZERO, Vec3::X);
        assert!(spherecast(&[], &hulls, o, d, 0.4, 10.0, &[], !0).is_none());
        let hit = capsulecast(&[], &hulls, o, d, 0.4, 1.8, Vec3::Y, 10.0, &[], !0)
            .expect("the capsule's top cap reaches it");
        assert_eq!(hit.eid, Some(1));
    }

    /// The broadphase must not change the answer (`floptle/0076`).
    ///
    /// This is the only property that makes an index safe to drop under a solver:
    /// a candidate list that misses a collider is a body falling through the
    /// floor, and the failure appears at whatever scene size first spreads the
    /// geometry out. So the same fall is simulated against two worlds — one whose
    /// colliders all report bounds, one where they are all unbounded (and are
    /// therefore always candidates, i.e. the old full scan) — and the resting
    /// position has to match exactly.
    #[test]
    fn the_broadphase_finds_exactly_what_the_full_scan_found() {
        /// A box that refuses to say how big it is, so the index has to offer it
        /// to every query — the old behaviour, reproduced on purpose.
        use crate::shapes::BoxShape;
        struct Unbounded(BoxShape);
        impl crate::shapes::CollisionShape for Unbounded {
            fn distance(&self, p: Vec3) -> f32 {
                self.0.distance(p)
            }
            fn normal(&self, p: Vec3) -> Vec3 {
                self.0.normal(p)
            }
            // bounds() left at the default None.
        }

        // A floor built from many tiles, so a body only ever touches a few — the
        // case where an index either narrows correctly or breaks.
        let tiles: Vec<(Vec3, Vec3)> = (-6..=6)
            .flat_map(|x| {
                (-6..=6).map(move |z| {
                    (Vec3::new(x as f32 * 4.0, 0.0, z as f32 * 4.0), Vec3::new(2.0, 0.5, 2.0))
                })
            })
            .collect();

        let run = |bounded: bool| -> (Vec3, usize) {
            let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
            for (c, half) in &tiles {
                let b = BoxShape::new(*c, *half, Quat::IDENTITY);
                if bounded {
                    w.add_collider(Box::new(b));
                } else {
                    w.add_collider(Box::new(Unbounded(b)));
                }
            }
            let bi = w.add_body(Body::sphere(Vec3::new(3.0, 6.0, -5.0), 0.5));
            for _ in 0..240 {
                w.step(1.0 / 60.0);
            }
            (w.bodies[bi].pos, w.contacts.len())
        };

        let (with_index, n_index) = run(true);
        let (full_scan, n_scan) = run(false);
        assert_eq!(
            with_index, full_scan,
            "the indexed run rested at {with_index:?} and the full scan at {full_scan:?} —              a broadphase that changes the simulation is not a broadphase"
        );
        assert_eq!(n_index, n_scan, "…and it found the same number of contacts");
        assert!(with_index.y > 0.0, "the body should be resting ON the floor, not through it");
    }

    /// A body stepped one at a time (the rollback driver's path) must land in the
    /// same place as one stepped by the whole world.
    ///
    /// `step_body` bypasses `step`, so it also bypasses the index rebuild that
    /// lives there. If it used a stale or empty index the body would fall through
    /// — and the rollback acceptance test is that live and replayed ticks agree
    /// bit for bit.
    #[test]
    fn a_body_stepped_on_its_own_gets_the_same_broadphase() {
        use crate::shapes::BoxShape;
        let build = || {
            let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
            for x in -4..=4 {
                w.add_collider(Box::new(BoxShape::new(
                    Vec3::new(x as f32 * 3.0, 0.0, 0.0),
                    Vec3::new(1.5, 0.5, 8.0),
                    Quat::IDENTITY,
                )));
            }
            let bi = w.add_body(Body::sphere(Vec3::new(0.0, 5.0, 0.0), 0.5));
            (w, bi)
        };
        let (mut whole, bi) = build();
        for _ in 0..180 {
            whole.step(1.0 / 60.0);
        }
        let (mut one, bj) = build();
        for _ in 0..180 {
            one.step_body(bj, 1.0 / 60.0);
        }
        assert_eq!(
            whole.bodies[bi].pos, one.bodies[bj].pos,
            "stepping a body directly must not lose the broadphase"
        );
        assert!(whole.bodies[bi].pos.y > 0.0, "and it must still land on the floor");
    }
}
