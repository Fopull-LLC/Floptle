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
    /// The project's resolved layer table (names → bit indices + collision
    /// matrix), captured at build so runtime spawns and live `node.layer`
    /// edits resolve against the same table physics filters with.
    layers: floptle_core::Layers,
    /// Rebase policy (ADR-0015): when the focus (active camera) drifts past the
    /// threshold, the sim's local frame recenters on it between fixed steps.
    fo: floptle_core::FloatingOrigin,
    /// Each body's position at the START of the last gameplay tick (sim frame),
    /// aligned with `world.bodies` — [`Self::writeback_interpolated`] lerps
    /// `tick_prev → pos` so rendered motion spans the whole tick, not just the
    /// final physics substep. Empty until [`Self::step_tick`] first runs.
    tick_prev: Vec<Vec3>,
    /// Entity pairs touching as of the last tick (ordered `(min, max)` eids),
    /// with the most recent contact info — diffed each [`Self::step_tick`]
    /// into enter / stay / exit [`TouchEvent`]s.
    touching: std::collections::HashMap<(u32, u32), TouchInfo>,
    /// Events produced by the most recent tick(s), drained by the driver via
    /// [`Self::take_touch_events`] and dispatched to scripts.
    events: Vec<TouchEvent>,
}

/// The last known contact between a touching pair (world coordinates, so a
/// floating-origin rebase between ticks can't skew an exit event's point).
#[derive(Clone, Copy, Debug)]
struct TouchInfo {
    point: DVec3,
    normal: Vec3,
    sensor: bool,
}

/// Identity + filtering a static collider registers with: the source node's
/// layer bit, its entity index (what touch events name as the "other side"),
/// and whether it's a TRIGGER (events only, no blocking).
#[derive(Clone, Copy, Debug)]
pub struct StaticTag {
    pub layer: u8,
    pub eid: u32,
    pub sensor: bool,
}

/// Which edge of a touch a [`TouchEvent`] reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchPhase {
    /// The pair started touching this tick.
    Enter,
    /// Still touching (reported every tick while the contact lasts).
    Stay,
    /// The pair separated this tick (point/normal are the last known contact).
    Exit,
}

/// One collision/trigger event between two scene nodes, produced by
/// [`Sim::step_tick`] and dispatched to both nodes' scripts as
/// `onCollisionEnter/Stay/Exit` (solid) or `onTriggerEnter/Stay/Exit`
/// (`sensor` = a trigger collider was involved).
#[derive(Clone, Copy, Debug)]
pub struct TouchEvent {
    /// Entity indices of the two nodes (order not meaningful).
    pub a: u32,
    pub b: u32,
    pub phase: TouchPhase,
    /// Contact point, ABSOLUTE world coordinates.
    pub point: DVec3,
    /// Contact normal (unit), pointing out of the surface that was hit.
    pub normal: Vec3,
    /// A trigger (sensor) collider was involved — dispatch the trigger hooks.
    pub sensor: bool,
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
        let t: Vec<(DVec3, &Terrain, u8, Option<u32>)> =
            terrains.iter().map(|(a, t)| (*a, *t, 0, None)).collect();
        Self::build_layered(ecs, &t, gravity, origin, floptle_core::Layers::default())
    }

    /// [`Self::build`] with the project's layer table: each terrain tuple
    /// carries its node's resolved layer bit + entity (so touch events can
    /// name the terrain node), every `RigidBody` body resolves its node's
    /// named layer through `layers`, and the collision matrix lands in the
    /// physics world — so body-vs-collider pairs the project excepted never
    /// resolve, and masked raycasts filter with the same bits.
    pub fn build_layered(
        ecs: &World,
        terrains: &[(DVec3, &Terrain, u8, Option<u32>)],
        gravity: GravityField,
        origin: DVec3,
        layers: floptle_core::Layers,
    ) -> Self {
        let mut world = PhysicsWorld::new(gravity);
        world.origin = origin.round();
        world.matrix = layers.matrix;
        for (anchor, t, layer, eid) in terrains {
            world.add_collider_tagged(
                *anchor,
                Box::new(SdfTerrain { terrain: (*t).clone() }),
                *layer,
                *eid,
                false,
            );
        }
        let mut map = Vec::new();
        // Collect first (immutable borrow of the ECS) then build the bodies. A Dynamic
        // or Kinematic `RigidBody` becomes a body; a STATIC one becomes a baked
        // immovable collider in the body's shape — no body at all, zero per-tick cost.
        // If the node is *also* flagged `Collidable`/`MeshCollider`, that marker is
        // ignored here (and the editor skips adding a static collider for it), so a
        // body never fights a static shape sitting on top of it.
        let found: Vec<(Entity, RigidBody)> =
            ecs.query::<RigidBody>().map(|(e, rb)| (e, *rb)).collect();
        for (e, rb) in found {
            if rb.mode == floptle_core::BodyMode::Static {
                Self::add_static_body_collider(&mut world, ecs, e, &rb, &layers);
                continue;
            }
            let (b, rot0) = Self::body_from(ecs, e, &rb, world.origin, &layers);
            map.push(BodyLink { entity: e, body: world.add_body(b), lock_rot: rb.lock_rot, rot0 });
        }
        Self {
            world,
            map,
            accum: 0.0,
            fixed_dt: 1.0 / 120.0,
            layers,
            fo: floptle_core::FloatingOrigin::default(),
            tick_prev: Vec::new(),
            touching: std::collections::HashMap::new(),
            events: Vec::new(),
        }
    }

    /// The layer table this sim resolves named layers with (the editor uses it
    /// to compute static colliders' layer bits with the same rules).
    pub fn layers(&self) -> &floptle_core::Layers {
        &self.layers
    }

    /// Build one `RigidBody` entity's dynamic body (sim frame) — shared by
    /// [`Self::build`] and runtime spawns ([`Self::add_body_for`]).
    fn body_from(
        ecs: &World,
        e: Entity,
        rb: &RigidBody,
        origin: DVec3,
        layers: &floptle_core::Layers,
    ) -> (Body, Quat) {
        let wt = world_transform(ecs, e);
        // Sim frame: subtract the origin in f64 FIRST, then narrow — the residual
        // is small and exact no matter how far out the node sits.
        let pos = (wt.translation - origin).as_vec3();
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
        b.layer = layers.index_for(ecs, e);
        b.kinematic = rb.mode == floptle_core::BodyMode::Kinematic;
        let rot0 = ecs.get::<Transform>(e).map(|t| t.rotation).unwrap_or(Quat::IDENTITY);
        (b, rot0)
    }

    /// A STATIC-mode `RigidBody`: bake an immovable collider in the body's
    /// shape (sphere / capsule / box, sized by its params) at the node's world
    /// pose — the cheapest way to make something solid. Touch events still
    /// name the node (`eid`); it just never simulates.
    fn add_static_body_collider(
        world: &mut PhysicsWorld,
        ecs: &World,
        e: Entity,
        rb: &RigidBody,
        layers: &floptle_core::Layers,
    ) {
        let wt = world_transform(ecs, e);
        let (layer, eid) = (layers.index_for(ecs, e), Some(e.index()));
        let r = rb.radius.max(0.01);
        match rb.kind {
            BodyKind::Sphere => {
                world.add_collider_tagged(
                    wt.translation,
                    Box::new(SphereShape { center: Vec3::ZERO, radius: r }),
                    layer,
                    eid,
                    false,
                );
            }
            BodyKind::Capsule => {
                let u = (wt.rotation * Vec3::Y).try_normalize().unwrap_or(Vec3::Y);
                let half = (rb.height.max(2.0 * r) * 0.5 - r).max(0.0);
                world.add_collider_tagged(
                    wt.translation,
                    Box::new(CapsuleShape { a: -u * half, b: u * half, radius: r }),
                    layer,
                    eid,
                    false,
                );
            }
            BodyKind::Box => {
                let s = wt.scale;
                let h = rb.half_extents;
                world.add_collider_tagged(
                    wt.translation,
                    Box::new(BoxShape::new(
                        Vec3::ZERO,
                        Vec3::new(h[0] * s.x, h[1] * s.y, h[2] * s.z),
                        wt.rotation,
                    )),
                    layer,
                    eid,
                    false,
                );
            }
        }
    }

    /// Register a body for a RUNTIME-SPAWNED `RigidBody` node (`net.spawn`, or
    /// a replicated spawn arriving mid-play) — the live-session counterpart of
    /// [`Self::build`]'s pass. No-op (false) if the entity has no RigidBody or
    /// already has a body.
    pub fn add_body_for(&mut self, e: Entity, ecs: &World) -> bool {
        if self.map.iter().any(|l| l.entity == e) {
            return false;
        }
        let Some(rb) = ecs.get::<RigidBody>(e).copied() else { return false };
        // A STATIC-mode spawn (net.spawn of a wall/prop) bakes its collider
        // live instead of registering a body.
        if rb.mode == floptle_core::BodyMode::Static {
            if self.world.colliders.iter().any(|c| c.eid == Some(e.index())) {
                return false;
            }
            Self::add_static_body_collider(&mut self.world, ecs, e, &rb, &self.layers);
            return true;
        }
        let (b, rot0) = Self::body_from(ecs, e, &rb, self.world.origin, &self.layers);
        let pos = b.pos;
        let bi = self.world.add_body(b);
        self.map.push(BodyLink { entity: e, body: bi, lock_rot: rb.lock_rot, rot0 });
        // Keep the render-interpolation anchors aligned mid-tick (they rebuild
        // from scratch at the next `step_tick` anyway).
        if self.tick_prev.len() == bi {
            self.tick_prev.push(pos);
        }
        true
    }

    /// Remove a runtime-despawned entity's body. Swap-remove keeps the body
    /// array dense; the displaced (last) body's link is re-pointed. Also drops
    /// any static collider the entity baked (a STATIC-mode body, or a spawned
    /// Collidable) — contacts referencing collider indices are transient
    /// (rebuilt every step), so the retain is safe between ticks.
    pub fn remove_body(&mut self, eid: u32) {
        self.world.colliders.retain(|c| c.eid != Some(eid));
        let Some(li) = self.map.iter().position(|l| l.entity.index() == eid) else { return };
        let bi = self.map[li].body;
        let last = self.world.bodies.len() - 1;
        self.world.bodies.swap_remove(bi);
        self.map.remove(li);
        for l in &mut self.map {
            if l.body == last {
                l.body = bi;
            }
        }
        // Both hold body indices from before the swap: contacts are transient
        // (rebuilt every step) and the interpolation anchors rebuild next tick
        // (writeback falls back to each body's own prev_pos meanwhile).
        self.world.contacts.clear();
        self.tick_prev.clear();
    }

    /// Register a static triangle-mesh collider — e.g. an imported map model the player
    /// can walk on. `anchor` is the mesh node's world translation (full `f64`); `verts`
    /// are baked RELATIVE to it, so a map placed a million units out collides exactly.
    /// `tag` carries the node's layer bit / entity / trigger flag ([`Self::tag_for`]).
    /// Call after [`build`](Self::build).
    pub fn add_static_mesh(
        &mut self,
        anchor: DVec3,
        verts: &[Vec3],
        indices: &[u32],
        tag: StaticTag,
    ) {
        if indices.len() >= 3 && !verts.is_empty() {
            self.world.add_collider_tagged(
                anchor,
                Box::new(TriMeshCollider::new(verts, indices)),
                tag.layer,
                Some(tag.eid),
                tag.sensor,
            );
        }
    }

    /// Register a static oriented-box collider (the "collidable" switch on a Cube node).
    /// `center` is the node's world translation (full `f64`).
    pub fn add_static_box(&mut self, center: DVec3, half: Vec3, rot: Quat, tag: StaticTag) {
        self.world.add_collider_tagged(
            center,
            Box::new(BoxShape::new(Vec3::ZERO, half, rot)),
            tag.layer,
            Some(tag.eid),
            tag.sensor,
        );
    }

    /// Register a static sphere collider (a collidable Sphere node).
    pub fn add_static_sphere(&mut self, center: DVec3, radius: f32, tag: StaticTag) {
        self.world.add_collider_tagged(
            center,
            Box::new(SphereShape { center: Vec3::ZERO, radius: radius.max(1e-3) }),
            tag.layer,
            Some(tag.eid),
            tag.sensor,
        );
    }

    /// Register a static capsule collider (a collidable Capsule node). `up` is the capsule
    /// axis (world space); `half_height` is center-to-endcap-center; `radius` its thickness.
    pub fn add_static_capsule(
        &mut self,
        center: DVec3,
        up: Vec3,
        half_height: f32,
        radius: f32,
        tag: StaticTag,
    ) {
        let u = up.try_normalize().unwrap_or(Vec3::Y);
        self.world.add_collider_tagged(
            center,
            Box::new(CapsuleShape { a: -u * half_height, b: u * half_height, radius: radius.max(1e-3) }),
            tag.layer,
            Some(tag.eid),
            tag.sensor,
        );
    }

    /// The identity a node's static collider registers with: its resolved
    /// layer bit, its entity (what touch events name), and whether it's a
    /// trigger (a `Trigger` component alongside `Collidable`).
    pub fn tag_for(&self, ecs: &World, e: Entity) -> StaticTag {
        StaticTag {
            layer: self.layers.index_for(ecs, e),
            eid: e.index(),
            sensor: ecs.get::<floptle_core::Trigger>(e).is_some(),
        }
    }

    /// The collision-layer bit a node resolves to under this sim's layer table
    /// (its named `Layer` component, else Default/0) — for the editor's static
    /// collider registration.
    pub fn layer_for(&self, ecs: &World, e: Entity) -> u8 {
        self.layers.index_for(ecs, e)
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
    /// Override the floating-origin rebase distance (default 4096 world
    /// units). Mostly for tests — games rarely need to touch it.
    pub fn set_origin_threshold(&mut self, threshold: f64) {
        self.fo.threshold = threshold;
    }

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
        // Accumulate contacts across the tick's substeps (each step clears its
        // own) so a one-substep graze still registers as a touch this tick.
        let mut tick_contacts: Vec<crate::body::Contact> = Vec::new();
        let mut tick_kin: Vec<(usize, u32, Vec3, Vec3)> = Vec::new();
        for _ in 0..n {
            self.world.step(self.fixed_dt);
            tick_contacts.extend(self.world.contacts.iter().copied());
            tick_kin.extend(self.world.kin_contacts.iter().copied());
        }
        self.detect_touches(&tick_contacts, &tick_kin);
    }

    /// Diff this tick's touching pairs against the last tick's into
    /// enter / stay / exit [`TouchEvent`]s. Three sources, all matrix-gated:
    /// the solver's resolved contacts (body vs solid collider), body-vs-SENSOR
    /// overlap (triggers — the solver never resolves those), and body-vs-body
    /// hull overlap (the solver has no body-body response, but games still
    /// need to KNOW two bodies met). Costs O(contacts + bodies×sensors +
    /// bodies²) per tick — trivial at gameplay body counts.
    fn detect_touches(
        &mut self,
        tick_contacts: &[crate::body::Contact],
        tick_kin: &[(usize, u32, Vec3, Vec3)],
    ) {
        let origin = self.world.origin;
        let to_world = |p: Vec3| origin + DVec3::new(p.x as f64, p.y as f64, p.z as f64);
        // Body slot → entity index (slots without a link never event).
        let mut body_eid: Vec<Option<u32>> = vec![None; self.world.bodies.len()];
        for l in &self.map {
            if let Some(slot) = body_eid.get_mut(l.body) {
                *slot = Some(l.entity.index());
            }
        }
        let mut now: std::collections::HashMap<(u32, u32), TouchInfo> =
            std::collections::HashMap::new();
        let mut record = |a: u32, b: u32, info: TouchInfo| {
            let key = (a.min(b), a.max(b));
            // A sensor overlap never downgrades a solid contact's info.
            now.entry(key).or_insert(info);
        };
        // 1. Solid contacts the solver resolved this tick.
        for c in tick_contacts {
            let (Some(Some(a)), Some(b)) =
                (body_eid.get(c.body), self.world.colliders.get(c.collider).and_then(|k| k.eid))
            else {
                continue;
            };
            record(*a, b, TouchInfo { point: to_world(c.point), normal: c.normal, sensor: false });
        }
        // 1b. Dynamic-vs-KINEMATIC resolutions (a player standing on a moving
        // platform) — recorded by the solver's kinematic-hull pass.
        for (bi, kin_eid, point, normal) in tick_kin {
            let Some(Some(a)) = body_eid.get(*bi) else { continue };
            record(
                *a,
                *kin_eid,
                TouchInfo { point: to_world(*point), normal: *normal, sensor: false },
            );
        }
        // 2. Trigger (sensor) overlap — the solver skips these, so test here.
        for col in self.world.colliders.iter().filter(|c| c.sensor) {
            let Some(b_eid) = col.eid else { continue };
            for (bi, body) in self.world.bodies.iter().enumerate() {
                let Some(Some(a_eid)) = body_eid.get(bi) else { continue };
                if !body.active
                    || (self.world.matrix[body.layer as usize] >> col.layer) & 1 == 0
                {
                    continue;
                }
                let (centers, n_c, radius) = body.sample_centers();
                for &c in &centers[..n_c] {
                    let d = col.distance(c);
                    if d < radius {
                        record(
                            *a_eid,
                            b_eid,
                            TouchInfo {
                                point: to_world(c),
                                normal: col.normal(c),
                                sensor: true,
                            },
                        );
                        break;
                    }
                }
            }
        }
        // 3. Body-vs-body overlap (detection only; no physical response).
        for i in 0..self.world.bodies.len() {
            let Some(Some(a_eid)) = body_eid.get(i).copied() else { continue };
            let a = &self.world.bodies[i];
            if !a.active {
                continue;
            }
            for j in (i + 1)..self.world.bodies.len() {
                let Some(Some(b_eid)) = body_eid.get(j).copied() else { continue };
                let b = &self.world.bodies[j];
                if !b.active || (self.world.matrix[a.layer as usize] >> b.layer) & 1 == 0 {
                    continue;
                }
                let hull = crate::world::BodyHull {
                    eid: b_eid,
                    pos: b.pos,
                    radius: b.radius,
                    shape: b.shape,
                    up: b.up,
                    layer: b.layer,
                };
                let (centers, n_c, radius) = a.sample_centers();
                for &c in &centers[..n_c] {
                    if hull.distance(c) < radius {
                        record(
                            a_eid,
                            b_eid,
                            TouchInfo {
                                point: to_world(c),
                                normal: hull.normal(c),
                                sensor: false,
                            },
                        );
                        break;
                    }
                }
            }
        }
        // Diff → events.
        for (&(a, b), info) in &now {
            let phase =
                if self.touching.contains_key(&(a, b)) { TouchPhase::Stay } else { TouchPhase::Enter };
            self.events.push(TouchEvent {
                a,
                b,
                phase,
                point: info.point,
                normal: info.normal,
                sensor: info.sensor,
            });
        }
        for (&(a, b), info) in &self.touching {
            if !now.contains_key(&(a, b)) {
                self.events.push(TouchEvent {
                    a,
                    b,
                    phase: TouchPhase::Exit,
                    point: info.point,
                    normal: info.normal,
                    sensor: info.sensor,
                });
            }
        }
        self.touching = now;
    }

    /// Drain the collision/trigger events produced since the last drain (the
    /// driver dispatches them to both nodes' scripts after each tick).
    pub fn take_touch_events(&mut self) -> Vec<TouchEvent> {
        std::mem::take(&mut self.events)
    }

    /// Write interpolated body transforms to the ECS: `alpha` in `[0,1)` is how far
    /// render time has progressed into the CURRENT gameplay tick, so each body renders
    /// `lerp(tick_start, tick_end, alpha)`. Pair with [`Self::step_tick`]; frames where
    /// no tick ran keep interpolating along the same span (alpha keeps growing).
    pub fn writeback_interpolated(&self, ecs: &mut World, alpha: f32) {
        let alpha = alpha.clamp(0.0, 1.0);
        for link in &self.map {
            let b = &self.world.bodies[link.body];
            if !b.active || b.kinematic {
                // Snapshot-driven / kinematic: the TRANSFORM is authoritative
                // (interp or scripts own it) — never write the body pose back.
                continue;
            }
            let from = self.tick_prev.get(link.body).copied().unwrap_or(b.prev_pos);
            let p = from.lerp(b.pos, alpha);
            self.write_one_transform(ecs, link, p);
        }
    }

    /// Advance ONE body by one gameplay tick (the prediction-replay driver,
    /// `docs/netcode-design.md` §6): runs the tick's physics substeps for just
    /// that body — exact, because the solver has no body-vs-body pass. No
    /// floating-origin rebase, no transform writeback. The render anchor
    /// (`tick_prev`) advances with each replayed tick: [`Self::restore_body`]
    /// parked it at the (rtt-old) server pose, and leaving it there would make
    /// [`Self::writeback_interpolated`] render the whole replay span backwards
    /// for a frame after every correction — a visible blip scaled by rtt.
    /// Capturing the pre-step pose per replayed tick leaves the anchor exactly
    /// one tick behind the final state, same as a normal [`Self::step_tick`].
    pub fn step_body_tick(&mut self, eid: u32, tick_dt: f32) {
        let Some(bi) = self.map.iter().find(|l| l.entity.index() == eid).map(|l| l.body) else {
            return;
        };
        if let Some(tp) = self.tick_prev.get_mut(bi) {
            *tp = self.world.bodies[bi].pos;
        }
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
                BodyHull {
                    eid: l.entity.index(),
                    pos,
                    radius: b.radius,
                    shape: b.shape,
                    up: b.up,
                    layer: b.layer,
                }
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
                // Live layer switches: `node.layer = "Ghost"` (or an Inspector
                // edit) re-resolves against the play-start layer table.
                let layer = self.layers.index_for(ecs, ent);
                // Live Dynamic ↔ Kinematic switches (`rig.kinematic = true` /
                // the Inspector's mode dropdown). Static is structural — the
                // editor rebuilds the sim for it.
                let kinematic = rb.mode == floptle_core::BodyMode::Kinematic;
                // KINEMATIC bodies follow their node's transform (scripts and
                // animation move the node; the sim just tracks it) — origin-
                // relative in f64 so far-out platforms stay exact. On clients,
                // snapshots drive the transform, so this ALSO keeps a
                // replicated platform's hull where players see it.
                let kin_pos = kinematic.then(|| {
                    (world_transform(ecs, ent).translation - self.world.origin).as_vec3()
                });
                let b = &mut self.world.bodies[bidx];
                b.layer = layer;
                if b.kinematic && !kinematic {
                    // Waking up into Dynamic: start from rest at the tracked pose.
                    b.vel = Vec3::ZERO;
                }
                b.kinematic = kinematic;
                if let Some(p) = kin_pos {
                    b.prev_pos = b.pos;
                    b.pos = p;
                }
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
        // Refresh the kinematic hulls dynamic bodies collide against (poses
        // were just synced above). Cheap: kinematic bodies are few.
        self.world.kin_hulls = self
            .world
            .bodies
            .iter()
            .enumerate()
            .filter(|(_, b)| b.kinematic)
            .filter_map(|(bi, b)| {
                let eid = self.map.iter().find(|l| l.body == bi)?.entity.index();
                Some(BodyHull {
                    eid,
                    pos: b.pos,
                    radius: b.radius,
                    shape: b.shape,
                    up: b.up,
                    layer: b.layer,
                })
            })
            .collect();
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
            if !b.active || b.kinematic {
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

#[cfg(test)]
mod runtime_body_tests {
    use super::*;

    fn world_with_bodies(n: usize) -> (World, Vec<Entity>) {
        let mut w = World::default();
        let mut ents = Vec::new();
        for i in 0..n {
            let e = w.spawn();
            w.insert(
                e,
                Transform::from_translation(DVec3::new(10.0 * i as f64, 5.0, 0.0)),
            );
            w.insert(e, RigidBody { gravity: true, ..Default::default() });
            ents.push(e);
        }
        (w, ents)
    }

    #[test]
    fn runtime_spawn_gets_a_live_body_and_despawn_removes_it() {
        let (mut ecs, ents) = world_with_bodies(2);
        let mut sim = Sim::build(&ecs, &[], GravityField::uniform(Vec3::new(0.0, -10.0, 0.0)), DVec3::ZERO);
        sim.step_tick(1.0 / 60.0, None);

        // A node spawns MID-PLAY (net.spawn): its body registers live.
        let spawned = ecs.spawn();
        ecs.insert(spawned, Transform::from_translation(DVec3::new(100.0, 5.0, 0.0)));
        ecs.insert(spawned, RigidBody { gravity: true, ..Default::default() });
        assert!(sim.add_body_for(spawned, &ecs), "spawned body must register");
        assert!(!sim.add_body_for(spawned, &ecs), "double-register is a no-op");
        let y0 = sim.body_snapshot(spawned.index()).unwrap().pos.y;
        for _ in 0..30 {
            sim.step_tick(1.0 / 60.0, None);
        }
        let after = sim.body_snapshot(spawned.index()).unwrap();
        assert!(after.pos.y < y0, "the spawned body must FALL (it simulates)");

        // Despawn the FIRST body: the swap-remove must not corrupt the others.
        let survivor_before = sim.body_snapshot(ents[1].index()).unwrap();
        sim.remove_body(ents[0].index());
        assert!(sim.body_snapshot(ents[0].index()).is_none(), "removed body is gone");
        let survivor = sim.body_snapshot(ents[1].index()).unwrap();
        assert_eq!(survivor.pos, survivor_before.pos, "survivor keeps ITS state after the swap");
        assert!(sim.body_snapshot(spawned.index()).is_some(), "the spawned one survives too");
        sim.step_tick(1.0 / 60.0, None); // and stepping after removal is sound
        sim.writeback_interpolated(&mut ecs, 0.5);
    }

    /// A reconcile correction restores the body to the (old) server pose and
    /// replays forward with `step_body_tick`. The render anchor must FOLLOW
    /// the replay: if it stays at the restored pose, the next frame renders
    /// the whole replay span backwards — the joiner-side while-moving jitter.
    #[test]
    fn replay_advances_the_render_anchor() {
        let (mut ecs, ents) = world_with_bodies(1);
        let e = ents[0];
        let step = 1.0 / 60.0;
        let mut sim =
            Sim::build(&ecs, &[], GravityField::uniform(Vec3::new(0.0, -10.0, 0.0)), DVec3::ZERO);
        for _ in 0..5 {
            sim.step_tick(step, None);
        }
        // Correction: rewind to a server pose FAR behind, then replay 6 ticks
        // (a realistic rtt span) with a big horizontal velocity.
        let server = BodySnapshot {
            pos: DVec3::new(0.0, 5.0, 0.0),
            vel: Vec3::new(30.0, 0.0, 0.0),
            grounded: false,
        };
        sim.restore_body(e.index(), &server);
        for _ in 0..6 {
            sim.step_body_tick(e.index(), step);
        }
        let end = sim.body_snapshot(e.index()).unwrap().pos;
        // Render at alpha 0 = the anchor itself. It must sit within ONE tick
        // of travel behind the final pose, not back at the restored pose
        // (6 ticks × 30 m/s = 3 m behind — the blip this guards against).
        sim.writeback_interpolated(&mut ecs, 0.0);
        let rendered = ecs.get::<Transform>(e).unwrap().translation;
        let lag = (end - rendered).length();
        assert!(
            lag <= 30.0 * step as f64 + 1e-3,
            "render anchor must trail by at most one tick after replay, lagged {lag} m"
        );
        assert!(
            (rendered - server.pos).length() > 1.0,
            "render anchor must have LEFT the restored server pose"
        );
    }

    /// The collision matrix end-to-end: a "Ghosts"-layer body falls straight
    /// through a "Walls"-layer box the project excepted, while a Default-layer
    /// body standing on the same box stays put — and a masked ray skips the
    /// box exactly like the solver does.
    #[test]
    fn excepted_layers_pass_through_each_other() {
        let layers = floptle_core::Layers::resolve(
            vec!["Default".into(), "Ghosts".into(), "Walls".into()],
            &[("Ghosts".into(), "Walls".into())],
        );
        let mut ecs = World::default();
        let mk = |ecs: &mut World, x: f64, layer: Option<&str>| {
            let e = ecs.spawn();
            ecs.insert(e, Transform::from_translation(DVec3::new(x, 5.0, 0.0)));
            ecs.insert(e, RigidBody { gravity: true, ..Default::default() });
            if let Some(l) = layer {
                ecs.insert(e, floptle_core::Layer(l.to_string()));
            }
            e
        };
        let ghost = mk(&mut ecs, 0.0, Some("Ghosts"));
        let walker = mk(&mut ecs, 1.0, None);
        let mut sim = Sim::build_layered(
            &ecs,
            &[],
            GravityField::uniform(Vec3::new(0.0, -10.0, 0.0)),
            DVec3::ZERO,
            layers,
        );
        let wall_layer = sim.layers().index_of("Walls").unwrap();
        sim.add_static_box(DVec3::new(0.0, 2.0, 0.0), Vec3::new(50.0, 0.5, 50.0), Quat::IDENTITY, StaticTag { layer: wall_layer, eid: 999, sensor: false });
        for _ in 0..180 {
            sim.step_tick(1.0 / 60.0, None);
        }
        let g = sim.body_snapshot(ghost.index()).unwrap();
        let w = sim.body_snapshot(walker.index()).unwrap();
        assert!(g.pos.y < 0.0, "the Ghosts body must fall THROUGH the Walls box, y = {}", g.pos.y);
        assert!((w.pos.y - 3.0).abs() < 0.1, "the Default body rests ON it, y = {}", w.pos.y);
        // Masked raycast: exclude the Walls bit and the ray passes through too.
        let down = Vec3::new(0.0, -1.0, 0.0);
        let from = Vec3::new(1.0, 5.0, 0.0);
        let all = crate::raycast_colliders(&sim.world.colliders, from, down, 20.0, !0);
        assert!(all.is_some(), "unmasked ray hits the box");
        let masked = crate::raycast_colliders(
            &sim.world.colliders,
            from,
            down,
            20.0,
            !(1u32 << wall_layer),
        );
        assert!(masked.is_none(), "masking out Walls makes the ray pass through");
    }

    /// Body modes end-to-end: a STATIC rigidbody becomes a baked collider
    /// (no body at all — a dynamic ball rests on it), and a KINEMATIC body
    /// never falls, follows its transform, and CARRIES a dynamic body resting
    /// on it (the moving-platform contract).
    #[test]
    fn static_and_kinematic_modes_work() {
        use floptle_core::BodyMode;
        let mut ecs = World::default();
        // A static box floor at y = 0 (mode = Static — collider, not a body).
        let floor = ecs.spawn();
        ecs.insert(floor, Transform::from_translation(DVec3::new(0.0, 0.0, 0.0)));
        ecs.insert(
            floor,
            RigidBody {
                kind: BodyKind::Box,
                mode: BodyMode::Static,
                half_extents: [50.0, 0.5, 50.0],
                ..Default::default()
            },
        );
        // A dynamic ball dropped onto it.
        let ball = ecs.spawn();
        ecs.insert(ball, Transform::from_translation(DVec3::new(0.0, 3.0, 0.0)));
        ecs.insert(ball, RigidBody { gravity: true, ..Default::default() });
        // A kinematic platform out at x = 20, with a second ball on top.
        let platform = ecs.spawn();
        ecs.insert(platform, Transform::from_translation(DVec3::new(20.0, 2.0, 0.0)));
        ecs.insert(
            platform,
            RigidBody {
                kind: BodyKind::Box,
                mode: BodyMode::Kinematic,
                half_extents: [2.0, 0.25, 2.0],
                ..Default::default()
            },
        );
        let rider = ecs.spawn();
        ecs.insert(rider, Transform::from_translation(DVec3::new(20.0, 3.5, 0.0)));
        ecs.insert(rider, RigidBody { gravity: true, ..Default::default() });

        let mut sim = Sim::build_layered(
            &ecs,
            &[],
            GravityField::uniform(Vec3::new(0.0, -10.0, 0.0)),
            DVec3::ZERO,
            floptle_core::Layers::default(),
        );
        // Static mode: NO body registered (that's the compute saving)…
        assert!(sim.body_snapshot(floor.index()).is_none(), "static mode must not be a body");
        // …but its collider exists and names the node.
        assert!(sim.world.colliders.iter().any(|c| c.eid == Some(floor.index())));

        // Run 2 s: the ball must come to rest ON the static box (y ≈ 1.0),
        // the platform must NOT fall, and the rider must rest on the platform.
        for _ in 0..120 {
            sim.sync_dynamic_params(&ecs);
            sim.step_tick(1.0 / 60.0, None);
            sim.writeback_interpolated(&mut ecs, 1.0);
        }
        let ball_y = sim.body_snapshot(ball.index()).unwrap().pos.y;
        assert!((ball_y - 1.0).abs() < 0.1, "ball rests on the static box, y = {ball_y}");
        let plat_y = ecs.get::<Transform>(platform).unwrap().translation.y;
        assert!((plat_y - 2.0).abs() < 1e-6, "kinematic never falls, y = {plat_y}");
        let rider_y = sim.body_snapshot(rider.index()).unwrap().pos.y;
        assert!((rider_y - 2.75).abs() < 0.1, "rider rests ON the platform, y = {rider_y}");

        // Move the platform up via its TRANSFORM (script-style): the sim
        // follows, and the rider is CARRIED up with it.
        for _ in 0..120 {
            if let Some(t) = ecs.get_mut::<Transform>(platform) {
                t.translation.y += 2.0 / 120.0; // +2 units over 2 s
            }
            sim.sync_dynamic_params(&ecs);
            sim.step_tick(1.0 / 60.0, None);
            sim.writeback_interpolated(&mut ecs, 1.0);
        }
        let rider_y = sim.body_snapshot(rider.index()).unwrap().pos.y;
        assert!(
            (rider_y - 4.75).abs() < 0.2,
            "the platform must CARRY the rider up (expected ≈ 4.75, got {rider_y})"
        );
        let plat_y = ecs.get::<Transform>(platform).unwrap().translation.y;
        assert!((plat_y - 4.0).abs() < 1e-4, "the transform stayed authoritative, y = {plat_y}");
    }

    /// The touch-event pipeline end-to-end: a body dropped onto a solid box
    /// fires Enter (then Stay) against the box's node; a body passing through
    /// a TRIGGER fires sensor Enter → Exit without ever being blocked; and
    /// two bodies crossing paths fire a body-vs-body Enter.
    #[test]
    fn touch_events_fire_enter_stay_and_exit() {
        let mut ecs = World::default();
        let faller = ecs.spawn();
        ecs.insert(faller, Transform::from_translation(DVec3::new(0.0, 3.0, 0.0)));
        ecs.insert(faller, RigidBody { gravity: true, ..Default::default() });
        let mut sim = Sim::build_layered(
            &ecs,
            &[],
            GravityField::uniform(Vec3::new(0.0, -10.0, 0.0)),
            DVec3::ZERO,
            floptle_core::Layers::default(),
        );
        // A solid floor box (node #100) and a trigger box (node #200) hanging
        // in the fall path at y ≈ 2 — the body must pass THROUGH the trigger.
        sim.add_static_box(
            DVec3::new(0.0, 0.0, 0.0),
            Vec3::new(50.0, 0.5, 50.0),
            Quat::IDENTITY,
            StaticTag { layer: 0, eid: 100, sensor: false },
        );
        sim.add_static_box(
            DVec3::new(0.0, 2.0, 0.0),
            Vec3::new(1.0, 0.2, 1.0),
            Quat::IDENTITY,
            StaticTag { layer: 0, eid: 200, sensor: true },
        );
        let feid = faller.index();
        let mut trigger_enter = false;
        let mut trigger_exit = false;
        let mut floor_enter = false;
        let mut floor_stays = 0;
        for _ in 0..120 {
            sim.step_tick(1.0 / 60.0, None);
            for ev in sim.take_touch_events() {
                let pair = (ev.a.min(ev.b), ev.a.max(ev.b));
                if pair == (feid.min(200), feid.max(200)) {
                    assert!(ev.sensor, "the trigger pair must report as a sensor event");
                    match ev.phase {
                        TouchPhase::Enter => trigger_enter = true,
                        TouchPhase::Exit => trigger_exit = true,
                        TouchPhase::Stay => {}
                    }
                }
                if pair == (feid.min(100), feid.max(100)) {
                    assert!(!ev.sensor);
                    match ev.phase {
                        TouchPhase::Enter => floor_enter = true,
                        TouchPhase::Stay => floor_stays += 1,
                        TouchPhase::Exit => {}
                    }
                }
            }
        }
        assert!(trigger_enter, "falling through the trigger must fire onTriggerEnter");
        assert!(trigger_exit, "leaving the trigger must fire onTriggerExit");
        assert!(floor_enter, "landing on the floor must fire onCollisionEnter");
        assert!(floor_stays > 10, "resting on the floor keeps reporting Stay, got {floor_stays}");
        let rest = sim.body_snapshot(feid).unwrap().pos.y;
        assert!(rest < 2.0, "the trigger must NOT have blocked the fall, rested at y = {rest}");

        // Body-vs-body: drop a second body onto the resting one.
        let bomber = ecs.spawn();
        ecs.insert(bomber, Transform::from_translation(DVec3::new(0.0, 4.0, 0.0)));
        ecs.insert(bomber, RigidBody { gravity: true, ..Default::default() });
        assert!(sim.add_body_for(bomber, &ecs));
        let mut body_pair = false;
        for _ in 0..120 {
            sim.step_tick(1.0 / 60.0, None);
            for ev in sim.take_touch_events() {
                let pair = (ev.a.min(ev.b), ev.a.max(ev.b));
                if pair == (feid.min(bomber.index()), feid.max(bomber.index()))
                    && ev.phase == TouchPhase::Enter
                {
                    body_pair = true;
                }
            }
        }
        assert!(body_pair, "two bodies meeting must fire a body-vs-body Enter");
    }

    /// The floating-origin rebase must be INVISIBLE to rendering: a body
    /// cruising past the threshold renders at exactly `velocity × dt`
    /// increments straight through the rebase tick — never a snap. (The
    /// "world moves around the player" feature: a threshold rebase onto
    /// whole-number origins is EXACT in f32 — bodies, anchors, and the
    /// interpolation span all shift together, and the ECS stays absolute
    /// f64. This guards that contract.)
    #[test]
    fn origin_rebase_never_snaps_the_rendered_motion() {
        let (mut ecs, ents) = world_with_bodies(1);
        let e = ents[0];
        let step = 1.0 / 60.0;
        // Zero gravity: the body cruises +x at a constant 30 m/s.
        let mut sim = Sim::build(&ecs, &[], GravityField::uniform(Vec3::ZERO), DVec3::ZERO);
        sim.set_origin_threshold(50.0);
        sim.set_body_velocity(e.index(), Vec3::new(30.0, 0.0, 0.0));

        let mut last_x: Option<f64> = None;
        let mut origins = std::collections::HashSet::new();
        // 8 s → 240 m of travel → several rebases at the 50 m threshold.
        for _ in 0..480 {
            let focus = sim.body_snapshot(e.index()).unwrap().pos;
            sim.step_tick(step, Some(focus));
            origins.insert(format!("{:?}", sim.world.origin));
            // Two render samples per tick, like frames landing mid-tick.
            for alpha in [0.25f32, 0.75] {
                sim.writeback_interpolated(&mut ecs, alpha);
                let x = ecs.get::<Transform>(e).unwrap().translation.x;
                if let Some(prev) = last_x {
                    let d = x - prev;
                    assert!(
                        (-1e-3..=30.0 * step as f64 + 1e-3).contains(&d),
                        "rendered motion must be smooth across rebases: stepped {d} m \
                         (one tick of travel is {} m)",
                        30.0 * step as f64
                    );
                }
                last_x = Some(x);
            }
        }
        assert!(origins.len() >= 3, "the rebase must actually have fired: {origins:?}");
    }
}
