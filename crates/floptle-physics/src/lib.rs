//! # floptle-physics
//!
//! Floptle's distinctive physics need — letting players drive, roll, and roam on
//! **fractals that are actively morphing** — is exactly the case off-the-shelf
//! rigid-body engines are *worst* at (they assume explicit, mostly-static
//! collision geometry). So the collision core is custom and **SDF-first**: we
//! collide against the same signed-distance function the renderer draws, which
//! is cheaper AND more robust than re-meshing a morphing surface every frame.
//! See `docs/subsystems/physics.md` + ADR-0012.
//!
//! Layered design (own the novel parts, borrow only the boring parts):
//! - `world`     : the collision world — a set of colliders queried each step.
//! - `sdf`       : SDF colliders (fractals + analytic primitives); point/sphere/
//!                 capsule/ray queries via `f(p,t)`, normals from its gradient,
//!                 surface velocity from ∂f/∂t (so riders inherit the morph).
//! - `field`     : baked sparse SDF/voxel grid — decouples physics cost from the
//!                 expensive analytic fractal (analytic near, baked far).
//! - `mesh`      : triangle-BVH colliders for static/imported (Blender) meshes.
//! - `character` : kinematic capsule controller (the "cool movement system");
//!                 samples `gravity` and aligns orientation to `-g`, so you can
//!                 run on a fractal and up its swirling walls (ADR-0014).
//! - `vehicle`   : raycast-vehicle model (drive a car across the fractal).
//! - `gravity`   : gravity as a composable vector field `g(p)` — global, analytic
//!                 sources (planets), SDF-surface (`-∇f`), and calculated
//!                 density-field (Poisson `∇²Φ=4πGρ`, Barnes-Hut/FFT) (ADR-0014).
//! - `dynamics`  : OPTIONAL lightweight impulse solver for object-vs-object.

//! ## Slice 1 (this module): collision core + integrator
//! The foundational, headless-testable layer — see `docs/subsystems/physics-slices.md`.
//! A [`CollisionShape`] trait (analytic primitives + an SDF-terrain collider), a
//! composable [`GravityField`], and a [`PhysicsWorld`] of dynamic sphere [`Body`]s
//! advanced on a fixed timestep with penetration resolution. Editor/ECS wiring,
//! capsule character controllers, triggers, and mesh colliders are later slices.

use floptle_core::math::{DVec3, EulerRot, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_core::{world_transform, BodyKind, Entity, RigidBody, World};
use floptle_field::Terrain;

/// Anything physics can query: a signed distance field with a surface normal.
/// Distance is **positive outside** the solid (in air) and **negative inside**.
/// (A morph-time `t` parameter for fractals is added in a later slice.)
pub trait CollisionShape {
    /// Signed distance from world point `p` to the surface (positive = outside).
    fn distance(&self, p: Vec3) -> f32;
    /// Outward unit surface normal at `p` (direction of increasing distance).
    fn normal(&self, p: Vec3) -> Vec3;
}

/// A signed-distance query result: distance to surface + the outward normal.
#[derive(Debug, Clone, Copy)]
pub struct SdfHit {
    pub distance: f32,
    pub normal: [f32; 3],
}

/// A half-space (infinite floor/wall): solid on the `-normal` side of `point`.
pub struct Plane {
    pub point: Vec3,
    pub normal: Vec3,
}

impl Plane {
    /// A horizontal ground plane at height `y` (solid below, air above).
    pub fn ground(y: f32) -> Self {
        Self { point: Vec3::new(0.0, y, 0.0), normal: Vec3::Y }
    }
}

impl CollisionShape for Plane {
    fn distance(&self, p: Vec3) -> f32 {
        (p - self.point).dot(self.normal.try_normalize().unwrap_or(Vec3::Y))
    }
    fn normal(&self, _p: Vec3) -> Vec3 {
        self.normal.try_normalize().unwrap_or(Vec3::Y)
    }
}

/// A solid analytic sphere — e.g. a planet body to walk on.
pub struct SphereShape {
    pub center: Vec3,
    pub radius: f32,
}

impl CollisionShape for SphereShape {
    fn distance(&self, p: Vec3) -> f32 {
        (p - self.center).length() - self.radius
    }
    fn normal(&self, p: Vec3) -> Vec3 {
        (p - self.center).try_normalize().unwrap_or(Vec3::Y)
    }
}

/// An SDF-terrain collider — collides against the **same baked field the renderer
/// draws** (ADR-0012), in the terrain's local space. Owns a snapshot of the field so
/// the physics step is independent of editor state.
pub struct SdfTerrain {
    pub terrain: floptle_field::Terrain,
}

impl CollisionShape for SdfTerrain {
    fn distance(&self, p: Vec3) -> f32 {
        self.terrain.sample([p.x, p.y, p.z])
    }
    fn normal(&self, p: Vec3) -> Vec3 {
        Vec3::from(self.terrain.normal([p.x, p.y, p.z])).try_normalize().unwrap_or(Vec3::Y)
    }
}

/// One contribution to the composable gravity field (ADR-0014). A body sums the
/// enabled sources at its position and treats the result as "down".
pub enum GravitySource {
    /// Constant acceleration — most games, e.g. `(0, -9.81, 0)`.
    Uniform(Vec3),
    /// Constant-magnitude radial pull toward a point — a planet (Mario Galaxy):
    /// stand anywhere on a sphere world and "down" is toward its center. `radius`
    /// bounds the gravity well (≤ 0 = unbounded), so multiple planets don't fight.
    Point { center: Vec3, strength: f32, radius: f32 },
    /// Pull onto a collider's surface along `-∇f` — grounds you on fractal walls.
    SdfSurface { collider: usize, strength: f32 },
}

/// Gravity as a sum of composable sources `g(p)`.
#[derive(Default)]
pub struct GravityField {
    pub sources: Vec<GravitySource>,
}

impl GravityField {
    /// A single uniform gravity vector (the common case).
    pub fn uniform(g: Vec3) -> Self {
        Self { sources: vec![GravitySource::Uniform(g)] }
    }

    /// The summed acceleration at world point `p` (colliders are needed for the
    /// `SdfSurface` tier).
    pub fn accel_at(&self, p: Vec3, colliders: &[Box<dyn CollisionShape>]) -> Vec3 {
        let mut a = Vec3::ZERO;
        for s in &self.sources {
            a += match s {
                GravitySource::Uniform(g) => *g,
                GravitySource::Point { center, strength, radius } => {
                    let to = *center - p;
                    if *radius > 0.0 && to.length() > *radius {
                        Vec3::ZERO
                    } else {
                        to.try_normalize().unwrap_or(Vec3::ZERO) * *strength
                    }
                }
                GravitySource::SdfSurface { collider, strength } => colliders
                    .get(*collider)
                    .map(|c| -c.normal(p) * *strength)
                    .unwrap_or(Vec3::ZERO),
            };
        }
        a
    }
}

/// The collision shape of a dynamic body.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BodyShape {
    /// A sphere of the body's `radius`.
    Sphere,
    /// A capsule: `radius` thick, with `half_height` from the center to each end-sphere
    /// center, aligned to the body's `up` (kept along −gravity, so it stands upright).
    Capsule { half_height: f32 },
}

/// A dynamic body — a sphere or capsule integrated + depenetrated each step.
#[derive(Clone, Copy, Debug)]
pub struct Body {
    pub pos: Vec3,
    pub vel: Vec3,
    pub radius: f32,
    pub shape: BodyShape,
    /// Capsule axis, kept aligned to −gravity each step.
    pub up: Vec3,
    /// Bounciness: 0 = no bounce, 1 = perfectly elastic.
    pub restitution: f32,
    /// Surface friction: 0 = frictionless ice, 1 = no sliding.
    pub friction: f32,
    /// Freeze world-axis translation (x, y, z) — e.g. lock Z for a 2.5D game.
    pub lock_pos: [bool; 3],
    /// Set each step when the body is resting on a surface that opposes gravity.
    pub grounded: bool,
    /// The contact normal from the most recent resolved collision this step (telegraph).
    pub contact: Option<Vec3>,
    home: Vec3, // captured spawn position, restored on locked axes
}

impl Body {
    pub fn sphere(pos: Vec3, radius: f32) -> Self {
        Self {
            pos,
            vel: Vec3::ZERO,
            radius,
            shape: BodyShape::Sphere,
            up: Vec3::Y,
            restitution: 0.0,
            friction: 0.3,
            lock_pos: [false; 3],
            grounded: false,
            contact: None,
            home: pos,
        }
    }

    /// A capsule body of total standing `height` (clamped to ≥ 2·radius).
    pub fn capsule(pos: Vec3, radius: f32, height: f32) -> Self {
        let half = (height.max(2.0 * radius) * 0.5 - radius).max(0.0);
        Self { shape: BodyShape::Capsule { half_height: half }, ..Self::sphere(pos, radius) }
    }

    /// The collision sphere centers (1 for a sphere, 2 for a capsule's ends).
    fn centers(&self) -> ([Vec3; 2], usize) {
        match self.shape {
            BodyShape::Sphere => ([self.pos, self.pos], 1),
            BodyShape::Capsule { half_height } => {
                ([self.pos - self.up * half_height, self.pos + self.up * half_height], 2)
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

fn axis(v: Vec3, i: usize) -> f32 {
    match i {
        0 => v.x,
        1 => v.y,
        _ => v.z,
    }
}
fn set_axis(v: &mut Vec3, i: usize, val: f32) {
    match i {
        0 => v.x = val,
        1 => v.y = val,
        _ => v.z = val,
    }
}

/// The collision world for one scene: a gravity field, a set of colliders, and the
/// dynamic bodies, advanced together on a fixed timestep.
#[derive(Default)]
pub struct PhysicsWorld {
    pub gravity: GravityField,
    pub colliders: Vec<Box<dyn CollisionShape>>,
    pub bodies: Vec<Body>,
    /// Contacts resolved on the most recent `step` (cleared each step).
    pub contacts: Vec<Contact>,
}

impl PhysicsWorld {
    pub fn new(gravity: GravityField) -> Self {
        Self { gravity, colliders: Vec::new(), bodies: Vec::new(), contacts: Vec::new() }
    }

    pub fn add_collider(&mut self, shape: Box<dyn CollisionShape>) -> usize {
        self.colliders.push(shape);
        self.colliders.len() - 1
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
            // Semi-implicit Euler: orient up to −gravity, integrate gravity, then move.
            let g = self.gravity.accel_at(self.bodies[bi].pos, &self.colliders);
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
                    let (centers, n_c) = self.bodies[bi].centers();
                    let radius = self.bodies[bi].radius;
                    for &c in &centers[..n_c] {
                        let pen = radius - self.colliders[ci].distance(c);
                        if pen <= 0.0 {
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

/// Drives a [`PhysicsWorld`] from the ECS each Play (Slice 3 bridge): builds bodies
/// from `RigidBody` entities + an SDF terrain collider, advances on a fixed-timestep
/// accumulator decoupled from render fps, and writes resolved positions back to the
/// entities' transforms.
/// One body's link back to its ECS entity, plus its rotation constraint state.
struct BodyLink {
    entity: Entity,
    body: usize,
    lock_rot: [bool; 3],
    /// Authored local rotation, restored on locked axes each writeback.
    rot0: Quat,
}

pub struct Sim {
    pub world: PhysicsWorld,
    map: Vec<BodyLink>,
    accum: f32,
    pub fixed_dt: f32,
}

impl Sim {
    /// Build the sim from the ECS: every `RigidBody` entity becomes a dynamic sphere at
    /// its world position; `terrain` (e.g. the editor's combined field) becomes the SDF
    /// collider. `gravity` is the scene's gravity field.
    pub fn build(ecs: &World, terrain: Option<&Terrain>, gravity: GravityField) -> Self {
        let mut world = PhysicsWorld::new(gravity);
        if let Some(t) = terrain {
            world.add_collider(Box::new(SdfTerrain { terrain: t.clone() }));
        }
        let mut map = Vec::new();
        // Collect first (immutable borrow of the ECS) then build the bodies.
        let found: Vec<(Entity, RigidBody)> =
            ecs.query::<RigidBody>().map(|(e, rb)| (e, *rb)).collect();
        for (e, rb) in found {
            let wt = world_transform(ecs, e);
            let p = wt.translation;
            let pos = Vec3::new(p.x as f32, p.y as f32, p.z as f32);
            let r = rb.radius.max(0.01);
            let mut b = match rb.kind {
                BodyKind::Sphere => Body::sphere(pos, r),
                BodyKind::Capsule => Body::capsule(pos, r, rb.height),
            };
            b.restitution = rb.restitution;
            b.friction = rb.friction;
            b.lock_pos = rb.lock_pos;
            let rot0 = ecs.get::<Transform>(e).map(|t| t.rotation).unwrap_or(Quat::IDENTITY);
            map.push(BodyLink { entity: e, body: world.add_body(b), lock_rot: rb.lock_rot, rot0 });
        }
        Self { world, map, accum: 0.0, fixed_dt: 1.0 / 120.0 }
    }

    /// Advance by a (variable) real frame delta via a fixed-timestep accumulator, then
    /// write body positions back to the entities' local transform translations.
    /// (Physics bodies are treated as root nodes; parented dynamic bodies are later.)
    pub fn advance(&mut self, ecs: &mut World, real_dt: f32) {
        self.accum += real_dt.clamp(0.0, 0.25);
        let mut iters = 0;
        while self.accum >= self.fixed_dt && iters < 8 {
            self.world.step(self.fixed_dt);
            self.accum -= self.fixed_dt;
            iters += 1;
        }
        for link in &self.map {
            let p = self.world.bodies[link.body].pos;
            if let Some(t) = ecs.get_mut::<Transform>(link.entity) {
                t.translation = DVec3::new(p.x as f64, p.y as f64, p.z as f64);
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `world` for `secs` seconds at a fixed 1/120 s step.
    fn simulate(world: &mut PhysicsWorld, secs: f32) {
        let dt = 1.0 / 120.0;
        let steps = (secs / dt) as usize;
        for _ in 0..steps {
            world.step(dt);
        }
    }

    #[test]
    fn sphere_settles_on_ground() {
        let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
        w.add_collider(Box::new(Plane::ground(0.0)));
        let b = w.add_body(Body::sphere(Vec3::new(0.0, 5.0, 0.0), 0.5));
        simulate(&mut w, 3.0);
        let body = w.bodies[b];
        // Rests with its bottom on the floor: center at radius above y=0.
        assert!((body.pos.y - 0.5).abs() < 0.05, "y was {}", body.pos.y);
        assert!(body.vel.length() < 0.3, "vel {}", body.vel.length());
        assert!(body.grounded, "should be grounded");
    }

    #[test]
    fn sphere_slides_down_a_slope() {
        // A frictionless plane tilted ~20° (normal toward +X, so downhill is +X): a
        // body on it slides downhill and keeps accelerating.
        let theta = 20f32.to_radians();
        let normal = Vec3::new(theta.sin(), theta.cos(), 0.0);
        let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
        w.add_collider(Box::new(Plane { point: Vec3::ZERO, normal }));
        let mut body = Body::sphere(Vec3::new(0.0, 2.0, 0.0), 0.5);
        body.friction = 0.0; // ice, so it definitely slides
        let b = w.add_body(body);
        simulate(&mut w, 2.0);
        let body = w.bodies[b];
        // It should have slid downhill (+X for this normal) and be moving.
        assert!(body.pos.x > 0.5, "x was {}", body.pos.x);
        assert!(body.vel.x > 0.5, "vx was {}", body.vel.x);
    }

    #[test]
    fn radial_gravity_grounds_a_planet_from_any_side() {
        // A sphere "planet" of radius 3 at the origin, with radial gravity toward its
        // center. Bodies dropped from different sides all land ON the surface — the
        // out-of-the-box Mario-Galaxy case.
        let mut g = GravityField::default();
        g.sources.push(GravitySource::Point { center: Vec3::ZERO, strength: 12.0, radius: 0.0 });
        let mut w = PhysicsWorld::new(g);
        w.add_collider(Box::new(SphereShape { center: Vec3::ZERO, radius: 3.0 }));
        let top = w.add_body(Body::sphere(Vec3::new(0.0, 8.0, 0.0), 0.5));
        let side = w.add_body(Body::sphere(Vec3::new(8.0, 0.2, 0.0), 0.5));
        simulate(&mut w, 4.0);
        let r = 3.5; // planet radius + body radius
        let t = w.bodies[top];
        let s = w.bodies[side];
        assert!((t.pos.length() - r).abs() < 0.1, "top dist {}", t.pos.length());
        assert!(t.pos.y > 3.0, "top should rest on +Y side, y={}", t.pos.y);
        assert!((s.pos.length() - r).abs() < 0.1, "side dist {}", s.pos.length());
        assert!(s.pos.x > 3.0, "side should rest on +X side, x={}", s.pos.x);
    }

    #[test]
    fn sphere_settles_on_sdf_terrain() {
        // The SDF path: a flat terrain field (ground at y=0); a sphere drops onto it.
        let terrain =
            floptle_field::Terrain::flat([16, 16, 16], [0.0, 0.0, 0.0], [8.0, 8.0, 8.0], 0.0, [0.4, 0.6, 0.3]);
        let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
        w.add_collider(Box::new(SdfTerrain { terrain }));
        let b = w.add_body(Body::sphere(Vec3::new(0.0, 5.0, 0.0), 0.5));
        simulate(&mut w, 3.0);
        let body = w.bodies[b];
        assert!((body.pos.y - 0.5).abs() < 0.15, "y was {}", body.pos.y);
        assert!(body.vel.length() < 0.5, "vel {}", body.vel.length());
    }

    #[test]
    fn simulation_is_stable() {
        // Many steps must not blow up (NaN / runaway velocity).
        let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
        w.add_collider(Box::new(Plane::ground(0.0)));
        let b = w.add_body(Body::sphere(Vec3::new(0.0, 3.0, 0.0), 0.5));
        simulate(&mut w, 10.0);
        let body = w.bodies[b];
        assert!(body.pos.is_finite(), "pos {:?}", body.pos);
        assert!(body.vel.length() < 1.0, "vel {}", body.vel.length());
    }

    /// Run a character with a constant world-space move input for `secs`.
    fn walk(world: &PhysicsWorld, ch: &mut Character, input: Vec3, secs: f32) {
        let dt = 1.0 / 120.0;
        for _ in 0..(secs / dt) as usize {
            ch.update(world, input, dt);
        }
    }

    #[test]
    fn character_walks_flat_ground() {
        let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
        w.add_collider(Box::new(Plane::ground(0.0)));
        let mut ch = Character::new(Vec3::new(0.0, 2.0, 0.0), 0.4, 1.8);
        walk(&w, &mut ch, Vec3::new(1.0, 0.0, 0.0), 3.0);
        // Stands on the ground (foot ~y=0, no sinking) and walked +X, grounded.
        assert!(ch.pos.y.abs() < 0.06, "y {}", ch.pos.y);
        assert!(ch.pos.x > 3.0, "x {}", ch.pos.x);
        assert!(ch.grounded, "should be grounded");
    }

    #[test]
    fn character_circumnavigates_a_planet() {
        // Sphere planet (radius 5) + radial gravity. A constant +X world input walks
        // the character from the north pole toward the +X equator, following the
        // curved surface, staying grounded and upright — Mario Galaxy on foot.
        let mut g = GravityField::default();
        g.sources.push(GravitySource::Point { center: Vec3::ZERO, strength: 14.0, radius: 0.0 });
        let mut w = PhysicsWorld::new(g);
        w.add_collider(Box::new(SphereShape { center: Vec3::ZERO, radius: 5.0 }));
        let mut ch = Character::new(Vec3::new(0.0, 5.0, 0.0), 0.4, 1.6);
        walk(&w, &mut ch, Vec3::new(1.0, 0.0, 0.0), 8.0);
        // Moved a good way around (off the pole, toward +X), still on the surface.
        let dist = ch.pos.length();
        assert!((4.5..=5.6).contains(&dist), "dist from center {dist}");
        assert!(ch.pos.x > 2.0, "x {}", ch.pos.x);
        assert!(ch.pos.y < 4.5, "y {} (should have left the pole)", ch.pos.y);
        assert!(ch.grounded, "should stay grounded on the planet");
        // "Up" stays radial (outward from the planet center).
        let radial = ch.pos.normalize();
        assert!(ch.up.dot(radial) > 0.85, "up·radial {}", ch.up.dot(radial));
    }

    #[test]
    fn character_respects_slope_limit() {
        let gravity = Vec3::new(0.0, -9.81, 0.0);
        // Gentle slope (15°, within the 50° limit): the character is grounded.
        let gentle = (15f32).to_radians();
        let mut w = PhysicsWorld::new(GravityField::uniform(gravity));
        w.add_collider(Box::new(Plane {
            point: Vec3::ZERO,
            normal: Vec3::new(gentle.sin(), gentle.cos(), 0.0),
        }));
        let mut ch = Character::new(Vec3::new(0.0, 2.0, 0.0), 0.4, 1.6);
        walk(&w, &mut ch, Vec3::ZERO, 2.0);
        assert!(ch.grounded, "gentle slope should be standable");

        // Steep slope (70°, beyond the limit): not grounded → it slides down.
        let steep = (70f32).to_radians();
        let mut w2 = PhysicsWorld::new(GravityField::uniform(gravity));
        w2.add_collider(Box::new(Plane {
            point: Vec3::ZERO,
            normal: Vec3::new(steep.sin(), steep.cos(), 0.0),
        }));
        let mut ch2 = Character::new(Vec3::new(0.0, 2.0, 0.0), 0.4, 1.6);
        walk(&w2, &mut ch2, Vec3::ZERO, 2.0);
        assert!(!ch2.grounded, "steep slope must not ground the character");
        assert!(ch2.pos.x > 0.3, "should have slid downhill, x {}", ch2.pos.x);
    }

    #[test]
    fn sim_drops_a_rigidbody_onto_terrain() {
        // The full ECS bridge: a RigidBody entity above a flat terrain falls and settles
        // on it, with the result written back to the entity's transform.
        let terrain =
            Terrain::flat([16, 16, 16], [0.0, 0.0, 0.0], [8.0, 8.0, 8.0], 0.0, [0.4, 0.6, 0.3]);
        let mut ecs = World::default();
        let e = ecs.spawn();
        ecs.insert(e, Transform::from_translation(DVec3::new(0.0, 5.0, 0.0)));
        ecs.insert(e, RigidBody { radius: 0.5, ..Default::default() });

        let mut sim = Sim::build(&ecs, Some(&terrain), GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
        for _ in 0..240 {
            sim.advance(&mut ecs, 1.0 / 60.0);
        }
        let y = ecs.get::<Transform>(e).unwrap().translation.y;
        assert!((y - 0.5).abs() < 0.15, "entity settled at y={y}, expected ~0.5");
    }

    #[test]
    fn capsule_settles_upright_on_ground() {
        // A capsule (radius 0.4, height 2.0) rests with its foot on the floor: its
        // center ends up at half_height + radius = 0.6 + 0.4 = 1.0.
        let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
        w.add_collider(Box::new(Plane::ground(0.0)));
        let b = w.add_body(Body::capsule(Vec3::new(0.0, 5.0, 0.0), 0.4, 2.0));
        simulate(&mut w, 3.0);
        let body = w.bodies[b];
        assert!((body.pos.y - 1.0).abs() < 0.08, "capsule center y {}", body.pos.y);
        assert!(body.grounded, "capsule should be grounded");
    }

    #[test]
    fn lock_pos_freezes_an_axis() {
        // Lock X: a +X shove can't move the body off x=0, but it still falls in Y.
        let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
        w.add_collider(Box::new(Plane::ground(0.0)));
        let mut body = Body::sphere(Vec3::new(0.0, 5.0, 0.0), 0.5);
        body.lock_pos[0] = true;
        body.vel = Vec3::new(8.0, 0.0, 0.0); // shove +X
        let b = w.add_body(body);
        simulate(&mut w, 2.0);
        let body = w.bodies[b];
        assert!(body.pos.x.abs() < 1e-3, "x should stay locked at 0, was {}", body.pos.x);
        assert!((body.pos.y - 0.5).abs() < 0.05, "should still fall, y {}", body.pos.y);
    }

    #[test]
    fn contacts_are_recorded() {
        // A resting body produces a contact each step (for telegraphing/events).
        let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
        w.add_collider(Box::new(Plane::ground(0.0)));
        w.add_body(Body::sphere(Vec3::new(0.0, 1.0, 0.0), 0.5));
        simulate(&mut w, 1.0);
        w.step(1.0 / 120.0); // one more step to capture the contact
        assert!(!w.contacts.is_empty(), "a resting body should report a contact");
        assert!(w.contacts[0].normal.y > 0.9, "ground contact normal up, {:?}", w.contacts[0].normal);
    }
}
