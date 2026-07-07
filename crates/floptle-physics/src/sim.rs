//! The scene-facing simulation wrapper: builds bodies/colliders from the
//! ECS World's RigidBody nodes, steps physics, and writes transforms back
//! (origin-relative for the floating origin).

use floptle_core::math::{DVec3, EulerRot, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_core::{world_transform, BodyKind, Entity, RigidBody, World};
use floptle_field::Terrain;

use crate::body::{Body, BodyShape};
use crate::gravity::GravityField;
use crate::shapes::{BoxShape, CapsuleShape, SdfTerrain, SphereShape, TriMeshCollider};
use crate::world::{BodyHull, PhysicsWorld, RayHit};

/// Drives a [`PhysicsWorld`] from the ECS each Play (Slice 3 bridge): builds bodies
/// from `RigidBody` entities + an SDF terrain collider, advances on a fixed-timestep
/// accumulator decoupled from render fps, and writes resolved positions back to the
/// entities' transforms.
/// One body's link back to its ECS entity, plus its rotation constraint state.
struct BodyLink {
    entity: Entity,
    body: usize,
    lock_rot: [bool; 3],
    /// Local rotation restored on locked axes each writeback: the authored one,
    /// re-captured when a rotation lock engages mid-play (freeze in place).
    rot0: Quat,
}

pub struct Sim {
    pub world: PhysicsWorld,
    map: Vec<BodyLink>,
    accum: f32,
    pub fixed_dt: f32,
    /// Rebase policy (ADR-0015): when the focus (active camera) drifts past the
    /// threshold, the sim's local frame recenters on it between fixed steps.
    fo: floptle_core::FloatingOrigin,
    /// Each body's position at the START of the last gameplay tick (sim frame),
    /// aligned with `world.bodies` — [`Self::writeback_interpolated`] lerps
    /// `tick_prev → pos` so rendered motion spans the whole tick, not just the
    /// final physics substep. Empty until [`Self::step_tick`] first runs.
    tick_prev: Vec<Vec3>,
}

/// One body's full dynamic state, in ABSOLUTE world coordinates (f64 position,
/// floating-origin safe) — the serializable capture the netcode snapshots and
/// prediction rollback restore. See `docs/netcode-design.md` §2/§6.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BodySnapshot {
    /// World-space position (absolute — NOT sim-frame/origin-relative).
    pub pos: DVec3,
    pub vel: Vec3,
    pub grounded: bool,
}

impl Sim {
    /// Build the sim from the ECS: every `RigidBody` entity becomes a dynamic sphere at
    /// its world position; each terrain volume in `terrains` — `(node world translation,
    /// node-local field)` — becomes its OWN anchored SDF collider, at the field's native
    /// resolution (no combined-grid resolution spread, and exact placement anywhere in
    /// the world). `gravity` is the scene's gravity field with `Point` centers already
    /// converted to the sim frame. `origin` is the world point the sim's local frame is
    /// centered on — pass the play-start focus (rounded to whole units) so the numbers
    /// physics differences stay small even when the scene content sits far out.
    pub fn build(
        ecs: &World,
        terrains: &[(DVec3, &Terrain)],
        gravity: GravityField,
        origin: DVec3,
    ) -> Self {
        let mut world = PhysicsWorld::new(gravity);
        world.origin = origin.round();
        for (anchor, t) in terrains {
            world.add_collider_at(*anchor, Box::new(SdfTerrain { terrain: (*t).clone() }));
        }
        let mut map = Vec::new();
        // Collect first (immutable borrow of the ECS) then build the bodies. A `RigidBody`
        // ALWAYS becomes a dynamic body — that's what makes it fall/move. If the node is
        // *also* flagged `Collidable`/`MeshCollider`, that marker is ignored here (and the
        // editor skips adding a static collider for it), so the dynamic body never fights a
        // static shape sitting on top of it. `Collidable` means "static world geometry" only
        // when there is NO RigidBody; the solver has no body-vs-body pass, so to make one
        // object a solid obstacle the player bumps into, make it Collidable with no RigidBody.
        let found: Vec<(Entity, RigidBody)> =
            ecs.query::<RigidBody>().map(|(e, rb)| (e, *rb)).collect();
        for (e, rb) in found {
            let wt = world_transform(ecs, e);
            // Sim frame: subtract the origin in f64 FIRST, then narrow — the residual
            // is small and exact no matter how far out the node sits.
            let pos = (wt.translation - world.origin).as_vec3();
            let r = rb.radius.max(0.01);
            let mut b = match rb.kind {
                BodyKind::Sphere => Body::sphere(pos, r),
                BodyKind::Capsule => Body::capsule(pos, r, rb.height),
                BodyKind::Box => {
                    let s = wt.scale;
                    let h = rb.half_extents;
                    Body::boxx(pos, Vec3::new(h[0] * s.x, h[1] * s.y, h[2] * s.z))
                }
            };
            b.restitution = rb.restitution;
            b.friction = rb.friction;
            b.use_gravity = rb.gravity;
            b.lock_pos = rb.lock_pos;
            let rot0 = ecs.get::<Transform>(e).map(|t| t.rotation).unwrap_or(Quat::IDENTITY);
            map.push(BodyLink { entity: e, body: world.add_body(b), lock_rot: rb.lock_rot, rot0 });
        }
        Self {
            world,
            map,
            accum: 0.0,
            fixed_dt: 1.0 / 120.0,
            fo: floptle_core::FloatingOrigin::default(),
            tick_prev: Vec::new(),
        }
    }

    /// Register a static triangle-mesh collider — e.g. an imported map model the player
    /// can walk on. `anchor` is the mesh node's world translation (full `f64`); `verts`
    /// are baked RELATIVE to it, so a map placed a million units out collides exactly.
    /// Call after [`build`](Self::build).
    pub fn add_static_mesh(&mut self, anchor: DVec3, verts: &[Vec3], indices: &[u32]) {
        if indices.len() >= 3 && !verts.is_empty() {
            self.world.add_collider_at(anchor, Box::new(TriMeshCollider::new(verts, indices)));
        }
    }

    /// Register a static oriented-box collider (the "collidable" switch on a Cube node).
    /// `center` is the node's world translation (full `f64`).
    pub fn add_static_box(&mut self, center: DVec3, half: Vec3, rot: Quat) {
        self.world.add_collider_at(center, Box::new(BoxShape::new(Vec3::ZERO, half, rot)));
    }

    /// Register a static sphere collider (a collidable Sphere node).
    pub fn add_static_sphere(&mut self, center: DVec3, radius: f32) {
        self.world
            .add_collider_at(center, Box::new(SphereShape { center: Vec3::ZERO, radius: radius.max(1e-3) }));
    }

    /// Register a static capsule collider (a collidable Capsule node). `up` is the capsule
    /// axis (world space); `half_height` is center-to-endcap-center; `radius` its thickness.
    pub fn add_static_capsule(&mut self, center: DVec3, up: Vec3, half_height: f32, radius: f32) {
        let u = up.try_normalize().unwrap_or(Vec3::Y);
        self.world.add_collider_at(
            center,
            Box::new(CapsuleShape { a: -u * half_height, b: u * half_height, radius: radius.max(1e-3) }),
        );
    }

    /// Cast a ray against the world's colliders (terrain + meshes) from a WORLD-space
    /// origin; the hit point comes back in world space too. See [`PhysicsWorld::raycast`].
    pub fn raycast(&self, origin: DVec3, dir: Vec3, max_dist: f32) -> Option<RayHit> {
        let o = (origin - self.world.origin).as_vec3();
        self.world.raycast(o, dir, max_dist).map(|mut h| {
            let p = self.world.origin + DVec3::new(h.point[0] as f64, h.point[1] as f64, h.point[2] as f64);
            h.point = [p.x as f32, p.y as f32, p.z as f32];
            h
        })
    }

    /// Advance by a (variable) real frame delta via a fixed-timestep accumulator, then
    /// write body positions back to the entities' local transform translations —
    /// interpolated by the accumulator's leftover fraction, so rendered motion is smooth
    /// at any frame rate. `focus` (the active camera, world space) drives the floating
    /// origin: drift past the threshold and the sim recenters on it between steps.
    /// (Physics bodies are treated as root nodes; parented dynamic bodies are later.)
    pub fn advance(&mut self, ecs: &mut World, real_dt: f32, focus: Option<DVec3>) {
        if let Some(f) = focus {
            let local = f - self.world.origin;
            if local.length_squared() >= self.fo.threshold * self.fo.threshold {
                let new_origin = f.round(); // whole numbers → the shift is exact in f32
                self.fo.total_shift += self.world.origin - new_origin;
                self.world.rebase(new_origin);
            }
        }
        self.accum += real_dt.clamp(0.0, 0.25);
        let mut iters = 0;
        while self.accum >= self.fixed_dt && iters < 8 {
            self.world.step(self.fixed_dt);
            self.accum -= self.fixed_dt;
            iters += 1;
        }
        // How far into the NEXT step real time has progressed (0..1): render that
        // fraction of the way from each body's previous step position to its current.
        let alpha = (self.accum / self.fixed_dt).clamp(0.0, 1.0);
        self.writeback_transforms(ecs, alpha);
    }

    /// Advance exactly ONE gameplay tick (`tick_dt`, e.g. 1/60): rebase the floating
    /// origin if the focus drifted, capture each body's tick-start position (the render
    /// interpolation anchor), then run the whole number of internal physics substeps
    /// that make up the tick (e.g. 2 × 1/120). NO transform writeback — the caller runs
    /// every banked tick, then calls [`Self::writeback_interpolated`] once per frame.
    ///
    /// This is the netcode-era driver (`docs/netcode-design.md` §3): the gameplay tick
    /// is the deterministic unit scripts' `fixedUpdate`, input commands, snapshots, and
    /// prediction all share, so physics must advance in exact tick multiples.
    pub fn step_tick(&mut self, tick_dt: f32, focus: Option<DVec3>) {
        if let Some(f) = focus {
            let local = f - self.world.origin;
            if local.length_squared() >= self.fo.threshold * self.fo.threshold {
                let new_origin = f.round(); // whole numbers → the shift is exact in f32
                self.fo.total_shift += self.world.origin - new_origin;
                self.world.rebase(new_origin);
            }
        }
        // Anchor AFTER any rebase so tick_prev and pos share the same frame.
        self.tick_prev.clear();
        self.tick_prev.extend(self.world.bodies.iter().map(|b| b.pos));
        let n = (tick_dt / self.fixed_dt).round().max(1.0) as u32;
        for _ in 0..n {
            self.world.step(self.fixed_dt);
        }
    }

    /// Write interpolated body transforms to the ECS: `alpha` in `[0,1)` is how far
    /// render time has progressed into the CURRENT gameplay tick, so each body renders
    /// `lerp(tick_start, tick_end, alpha)`. Pair with [`Self::step_tick`]; frames where
    /// no tick ran keep interpolating along the same span (alpha keeps growing).
    pub fn writeback_interpolated(&self, ecs: &mut World, alpha: f32) {
        let alpha = alpha.clamp(0.0, 1.0);
        for link in &self.map {
            let b = &self.world.bodies[link.body];
            if !b.active {
                continue; // snapshot-driven: interpolation owns its transform
            }
            let from = self.tick_prev.get(link.body).copied().unwrap_or(b.prev_pos);
            let p = from.lerp(b.pos, alpha);
            self.write_one_transform(ecs, link, p);
        }
    }

    /// Advance ONE body by one gameplay tick (the prediction-replay driver,
    /// `docs/netcode-design.md` §6): runs the tick's physics substeps for just
    /// that body — exact, because the solver has no body-vs-body pass. No
    /// floating-origin rebase, no tick_prev capture, no transform writeback:
    /// replay is invisible to rendering; the caller applies the final state.
    pub fn step_body_tick(&mut self, eid: u32, tick_dt: f32) {
        let Some(bi) = self.map.iter().find(|l| l.entity.index() == eid).map(|l| l.body) else {
            return;
        };
        let n = (tick_dt / self.fixed_dt).round().max(1.0) as u32;
        for _ in 0..n {
            self.world.step_body(bi, self.fixed_dt);
        }
    }

    /// Capture a body's full dynamic state by entity index, in absolute world
    /// coordinates — the serializable unit netcode snapshots (`docs/netcode-design.md`).
    pub fn body_snapshot(&self, eid: u32) -> Option<BodySnapshot> {
        self.map.iter().find(|l| l.entity.index() == eid).map(|l| {
            let b = &self.world.bodies[l.body];
            BodySnapshot {
                pos: self.world.origin
                    + DVec3::new(b.pos.x as f64, b.pos.y as f64, b.pos.z as f64),
                vel: b.vel,
                grounded: b.grounded,
            }
        })
    }

    /// Restore a body's dynamic state from a snapshot (prediction rollback / netcode
    /// apply). Converts the absolute position back into the sim frame and resets the
    /// interpolation anchors too, so a rollback never smears across the correction.
    pub fn restore_body(&mut self, eid: u32, s: &BodySnapshot) {
        for l in &self.map {
            if l.entity.index() == eid {
                let p = (s.pos - self.world.origin).as_vec3();
                let b = &mut self.world.bodies[l.body];
                b.pos = p;
                b.prev_pos = p;
                b.vel = s.vel;
                b.grounded = s.grounded;
                if let Some(tp) = self.tick_prev.get_mut(l.body) {
                    *tp = p;
                }
                return;
            }
        }
    }

    /// Per body: (entity, velocity, up, grounded, height) — so the editor can expose it
    /// to scripts (`up` is −gravity, for surface-relative movement on planets; `height`
    /// lets a controller read/animate its capsule height for crouching).
    pub fn body_states(&self) -> impl Iterator<Item = (Entity, Vec3, Vec3, bool, f32)> + '_ {
        self.map.iter().map(move |l| {
            let b = &self.world.bodies[l.body];
            (l.entity, b.vel, b.up, b.grounded, b.height())
        })
    }

    /// Raycastable hulls for every dynamic body, sim frame — lend to the script
    /// host alongside the colliders so `raycast(...)` can hit players/crates.
    /// Active bodies report the body's own position; INACTIVE bodies (networked
    /// authority nodes on a client — snapshots drive their transforms, the body
    /// is frozen) report the entity's interpolated transform instead, so rays
    /// hit them where the player actually SEES them.
    pub fn body_hulls(&self, ecs: &World) -> Vec<BodyHull> {
        self.map
            .iter()
            .map(|l| {
                let b = &self.world.bodies[l.body];
                let pos = if b.active {
                    b.pos
                } else {
                    ecs.get::<Transform>(l.entity)
                        .map(|t| (t.translation - self.world.origin).as_vec3())
                        .unwrap_or(b.pos)
                };
                BodyHull { eid: l.entity.index(), pos, radius: b.radius, shape: b.shape, up: b.up }
            })
            .collect()
    }

    /// Activate/deactivate a body by entity index. Inactive bodies neither
    /// step nor write back transforms — a networked client deactivates
    /// server-authoritative bodies so snapshots own their transforms.
    pub fn set_body_active(&mut self, eid: u32, active: bool) {
        for l in &self.map {
            if l.entity.index() == eid {
                self.world.bodies[l.body].active = active;
                return;
            }
        }
    }

    /// Set a body's velocity by its entity index (scripts write velocity each frame).
    pub fn set_body_velocity(&mut self, eid: u32, vel: Vec3) {
        for l in &self.map {
            if l.entity.index() == eid {
                self.world.bodies[l.body].vel = vel;
                return;
            }
        }
    }

    /// Re-read each dynamic body's tunable RigidBody params from the ECS (shape/size,
    /// friction, restitution, gravity on/off, position/rotation locks), WITHOUT resetting
    /// position or velocity. Lets the Inspector edit these live while playing — including
    /// dragging the radius/height/half-extents or switching shapes — with no teleport/reset.
    /// A lock that turns ON here freezes the body where it IS: the restored position/
    /// rotation re-captures from the current state, so toggling a lock mid-play (Inspector
    /// or a script's `rig.lock_y = true`) never teleports the body back to its spawn.
    pub fn sync_dynamic_params(&mut self, ecs: &World) {
        for i in 0..self.map.len() {
            let (ent, bidx) = (self.map[i].entity, self.map[i].body);
            if let Some(rb) = ecs.get::<RigidBody>(ent) {
                let newly_rot_locked =
                    (0..3).any(|a| rb.lock_rot[a] && !self.map[i].lock_rot[a]);
                if newly_rot_locked && let Some(t) = ecs.get::<Transform>(ent) {
                    // Locked axes were held at rot0 anyway, so a full re-capture keeps
                    // them while adopting the current angle on the newly-locked ones.
                    self.map[i].rot0 = t.rotation;
                }
                self.map[i].lock_rot = rb.lock_rot;
                let b = &mut self.world.bodies[bidx];
                for a in 0..3 {
                    if rb.lock_pos[a] && !b.lock_pos[a] {
                        crate::body::set_axis(&mut b.home, a, crate::body::axis(b.pos, a));
                    }
                }
                b.restitution = rb.restitution;
                b.friction = rb.friction;
                b.use_gravity = rb.gravity;
                b.lock_pos = rb.lock_pos;
                b.radius = rb.radius.max(0.01);
                b.shape = match rb.kind {
                    BodyKind::Sphere => BodyShape::Sphere,
                    BodyKind::Capsule => {
                        let half = (rb.height.max(2.0 * b.radius) * 0.5 - b.radius).max(0.0);
                        BodyShape::Capsule { half_height: half }
                    }
                    BodyKind::Box => {
                        let s = world_transform(ecs, ent).scale;
                        let h = rb.half_extents;
                        BodyShape::Box { half: Vec3::new(h[0] * s.x, h[1] * s.y, h[2] * s.z) }
                    }
                };
            }
        }
    }

    /// Set a capsule body's total standing height (for crouch). The feet stay planted —
    /// the body shrinks/grows from the top, so the center (and a camera at it) lowers
    /// when crouching. No-op on a sphere body or when the height is unchanged.
    pub fn set_body_height(&mut self, eid: u32, height: f32) {
        for l in &self.map {
            if l.entity.index() == eid {
                let b = &mut self.world.bodies[l.body];
                if let BodyShape::Capsule { half_height } = b.shape {
                    let r = b.radius;
                    let new_half = (height.max(2.0 * r) * 0.5 - r).max(0.0);
                    b.pos += b.up * (new_half - half_height); // keep feet planted
                    b.prev_pos += b.up * (new_half - half_height); // don't smear the resize
                    b.shape = BodyShape::Capsule { half_height: new_half };
                }
                return;
            }
        }
    }

    fn writeback_transforms(&self, ecs: &mut World, alpha: f32) {
        for link in &self.map {
            let b = &self.world.bodies[link.body];
            if !b.active {
                continue;
            }
            let p = b.prev_pos.lerp(b.pos, alpha);
            self.write_one_transform(ecs, link, p);
        }
    }

    /// Write one body's (interpolated) sim-frame position to its entity's transform,
    /// honoring the rotation-axis locks.
    fn write_one_transform(&self, ecs: &mut World, link: &BodyLink, p: Vec3) {
        if let Some(t) = ecs.get_mut::<Transform>(link.entity) {
            t.translation = self.world.origin + DVec3::new(p.x as f64, p.y as f64, p.z as f64);
            // Rotation constraints: keep the authored angle on each locked axis.
            if link.lock_rot.iter().any(|&l| l) {
                let (ay, ax, az) = t.rotation.to_euler(EulerRot::YXZ);
                let (by, bx, bz) = link.rot0.to_euler(EulerRot::YXZ);
                t.rotation = Quat::from_euler(
                    EulerRot::YXZ,
                    if link.lock_rot[1] { by } else { ay }, // Y axis
                    if link.lock_rot[0] { bx } else { ax }, // X axis
                    if link.lock_rot[2] { bz } else { az }, // Z axis
                );
            }
        }
    }
}
