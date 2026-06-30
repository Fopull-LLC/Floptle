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

use floptle_core::math::Vec3;

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
    /// stand anywhere on a sphere world and "down" is toward its center.
    Point { center: Vec3, strength: f32 },
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
                GravitySource::Point { center, strength } => {
                    (*center - p).try_normalize().unwrap_or(Vec3::ZERO) * *strength
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

/// A dynamic body — a sphere collider integrated each step.
#[derive(Clone, Copy, Debug)]
pub struct Body {
    pub pos: Vec3,
    pub vel: Vec3,
    pub radius: f32,
    /// Bounciness: 0 = no bounce, 1 = perfectly elastic.
    pub restitution: f32,
    /// Surface friction: 0 = frictionless ice, 1 = no sliding.
    pub friction: f32,
    /// Set each step when the body is resting on a surface that opposes gravity.
    pub grounded: bool,
}

impl Body {
    pub fn sphere(pos: Vec3, radius: f32) -> Self {
        Self { pos, vel: Vec3::ZERO, radius, restitution: 0.0, friction: 0.3, grounded: false }
    }
}

/// The collision world for one scene: a gravity field, a set of colliders, and the
/// dynamic bodies, advanced together on a fixed timestep.
#[derive(Default)]
pub struct PhysicsWorld {
    pub gravity: GravityField,
    pub colliders: Vec<Box<dyn CollisionShape>>,
    pub bodies: Vec<Body>,
}

impl PhysicsWorld {
    pub fn new(gravity: GravityField) -> Self {
        Self { gravity, colliders: Vec::new(), bodies: Vec::new() }
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
    /// via an accumulator) for stability, not the variable render delta.
    pub fn step(&mut self, dt: f32) {
        let dt = dt.clamp(0.0, 0.1); // guard against a huge stalled frame
        for body in &mut self.bodies {
            // Semi-implicit Euler: integrate gravity, then position.
            let g = self.gravity.accel_at(body.pos, &self.colliders);
            body.vel += g * dt;
            body.pos += body.vel * dt;
            body.grounded = false;

            // Resolve penetration against every collider (a couple of relaxation
            // passes so corners/overlaps settle).
            for _ in 0..2 {
                for shape in &self.colliders {
                    let pen = body.radius - shape.distance(body.pos);
                    if pen <= 0.0 {
                        continue;
                    }
                    let n = shape.normal(body.pos);
                    body.pos += n * pen; // push out to the surface
                    let vn = body.vel.dot(n);
                    if vn < 0.0 {
                        // Split into normal + tangential: reflect the normal part by
                        // restitution, damp the tangential part by friction.
                        let vt = body.vel - n * vn;
                        body.vel = vt * (1.0 - body.friction).clamp(0.0, 1.0) - n * vn * body.restitution;
                    }
                    // Grounded if this contact opposes gravity (a floor, not a wall).
                    let gd = self.gravity.accel_at(body.pos, &self.colliders);
                    if gd.length_squared() > 1e-6 && n.dot(-gd.normalize()) > 0.5 {
                        body.grounded = true;
                    }
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
        g.sources.push(GravitySource::Point { center: Vec3::ZERO, strength: 12.0 });
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
}
