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
//!   capsule/ray queries via `f(p,t)`, normals from its gradient,
//!   surface velocity from ∂f/∂t (so riders inherit the morph).
//! - `field`     : baked sparse SDF/voxel grid — decouples physics cost from the
//!   expensive analytic fractal (analytic near, baked far).
//! - `mesh`      : triangle-BVH colliders for static/imported (Blender) meshes.
//! - `character` : kinematic capsule controller (the "cool movement system");
//!   samples `gravity` and aligns orientation to `-g`, so you can
//!   run on a fractal and up its swirling walls (ADR-0014).
//! - `vehicle`   : raycast-vehicle model (drive a car across the fractal).
//! - `gravity`   : gravity as a composable vector field `g(p)` — global, analytic
//!   sources (planets), SDF-surface (`-∇f`), and calculated
//!   density-field (Poisson `∇²Φ=4πGρ`, Barnes-Hut/FFT) (ADR-0014).
//! - `dynamics`  : OPTIONAL lightweight impulse solver for object-vs-object.

//! ## Slice 1 (this module): collision core + integrator
//! The foundational, headless-testable layer — see `docs/subsystems/physics-slices.md`.
//! A [`CollisionShape`] trait (analytic primitives + an SDF-terrain collider), a
//! composable [`GravityField`], and a [`PhysicsWorld`] of dynamic sphere [`Body`]s
//! advanced on a fixed timestep with penetration resolution. Editor/ECS wiring,
//! capsule character controllers, triggers, and mesh colliders are later slices.


mod body;
mod character;
mod gravity;
mod shapes;
mod sim;
mod world;

pub use body::*;
pub use character::*;
pub use gravity::*;
pub use shapes::*;
pub use sim::*;
pub use world::*;

#[cfg(test)]
mod tests {
    use floptle_core::math::{DVec3, EulerRot, Quat, Vec3};
    use floptle_core::transform::Transform;
    use floptle_core::{BodyKind, RigidBody, World};
    use floptle_field::Terrain;

    use super::*;

    /// Run `world` for `secs` seconds at a fixed 1/120 s step.
    fn simulate(world: &mut PhysicsWorld, secs: f32) {
        let dt = 1.0 / 120.0;
        let steps = (secs / dt) as usize;
        for _ in 0..steps {
            world.step(dt);
        }
    }

    /// A flat quad (two triangles) in the XZ plane at height `y`, spanning ±`half`.
    fn floor_quad(y: f32, half: f32) -> (Vec<Vec3>, Vec<u32>) {
        let v = vec![
            Vec3::new(-half, y, -half),
            Vec3::new(half, y, -half),
            Vec3::new(half, y, half),
            Vec3::new(-half, y, half),
        ];
        (v, vec![0, 1, 2, 0, 2, 3])
    }

    #[test]
    fn trimesh_distance_and_normal() {
        let (v, i) = floor_quad(0.0, 5.0);
        let m = TriMeshCollider::new(&v, &i);
        // A point one unit above the quad: unsigned distance 1, normal points up.
        assert!((m.distance(Vec3::new(0.0, 1.0, 0.0)) - 1.0).abs() < 1e-3);
        assert!(m.normal(Vec3::new(0.0, 1.0, 0.0)).y > 0.9);
        // One unit below: distance 1, normal points down (push out the way it came).
        assert!((m.distance(Vec3::new(0.0, -1.0, 0.0)) - 1.0).abs() < 1e-3);
        assert!(m.normal(Vec3::new(0.0, -1.0, 0.0)).y < -0.9);
        // Far away (beyond the search block): reported as no-collision.
        assert!(m.distance(Vec3::new(0.0, 50.0, 0.0)) > 100.0);
    }

    #[test]
    fn degenerate_triangles_are_safe() {
        // A mesh with zero-area triangles (coincident / collinear verts) must not produce
        // NaN distances or corrupt a body resting on it.
        let mut v = vec![Vec3::new(-5.0, 0.0, -5.0), Vec3::new(5.0, 0.0, -5.0), Vec3::new(5.0, 0.0, 5.0), Vec3::new(-5.0, 0.0, 5.0)];
        let mut i = vec![0u32, 1, 2, 0, 2, 3];
        // Append degenerate tris: a point (all coincident) and a sliver (collinear).
        v.push(Vec3::new(1.0, 0.0, 1.0));
        i.extend_from_slice(&[4, 4, 4]); // zero triangle
        i.extend_from_slice(&[0, 1, 1]); // two coincident verts
        let m = TriMeshCollider::new(&v, &i);
        assert!(m.distance(Vec3::new(0.0, 1.0, 0.0)).is_finite());
        assert!(m.normal(Vec3::new(0.0, 1.0, 0.0)).is_finite());
        let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
        w.add_collider(Box::new(m));
        let b = w.add_body(Body::sphere(Vec3::new(0.0, 4.0, 0.0), 0.5));
        simulate(&mut w, 3.0);
        assert!(w.bodies[b].pos.is_finite(), "body went non-finite: {:?}", w.bodies[b].pos);
        assert!((w.bodies[b].pos.y - 0.5).abs() < 0.1, "y={}", w.bodies[b].pos.y);
    }

    #[test]
    fn sphere_settles_on_mesh_floor() {
        // A sphere dropped above a triangle-mesh floor comes to rest on top of it.
        let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
        let (v, i) = floor_quad(0.0, 6.0);
        w.add_collider(Box::new(TriMeshCollider::new(&v, &i)));
        let b = w.add_body(Body::sphere(Vec3::new(0.0, 4.0, 0.0), 0.5));
        simulate(&mut w, 3.0);
        let body = w.bodies[b];
        assert!((body.pos.y - 0.5).abs() < 0.08, "rests on mesh floor, y={}", body.pos.y);
        assert!(body.grounded, "should be grounded on the mesh");
    }

    #[test]
    fn box_shape_distance_and_normal() {
        // An axis-aligned 1×1×1 box (half 0.5) centered at the origin.
        let b = BoxShape::new(Vec3::ZERO, Vec3::splat(0.5), Quat::IDENTITY);
        assert!((b.distance(Vec3::new(0.0, 1.0, 0.0)) - 0.5).abs() < 1e-3, "face dist");
        assert!(b.distance(Vec3::ZERO) < 0.0, "inside is negative");
        assert!(b.normal(Vec3::new(0.0, 1.0, 0.0)).y > 0.9, "top normal points up");
        assert!(b.normal(Vec3::new(1.0, 0.0, 0.0)).x > 0.9, "side normal points out");
        // A rotated box still measures from its own faces.
        let r = BoxShape::new(Vec3::ZERO, Vec3::splat(0.5), Quat::from_rotation_y(0.5));
        assert!(r.distance(Vec3::new(0.0, 2.0, 0.0)).is_finite());
        assert!((r.distance(Vec3::new(0.0, 2.0, 0.0)) - 1.5).abs() < 1e-2, "y-face unaffected by yaw");
    }

    #[test]
    fn box_body_rests_on_floor() {
        // A box body dropped on a ground plane comes to rest with its base on the floor.
        let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
        w.add_collider(Box::new(Plane::ground(0.0)));
        let bi = w.add_body(Body::boxx(Vec3::new(0.0, 4.0, 0.0), Vec3::new(0.6, 0.4, 0.6)));
        simulate(&mut w, 3.0);
        let body = w.bodies[bi];
        assert!(body.pos.is_finite(), "box non-finite: {:?}", body.pos);
        // Center should sit ~half.y above the floor (base resting at y≈0).
        assert!((body.pos.y - 0.4).abs() < 0.12, "box rests on floor, y={}", body.pos.y);
        // At rest its velocity has been damped to ~zero (it's not falling through).
        assert!(body.vel.length() < 0.2, "box should be at rest, vel={:?}", body.vel);
    }

    #[test]
    fn static_box_collider_stops_falling_sphere() {
        // A sphere falls onto a static (collidable) box platform and rests on its top.
        let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
        w.add_collider(Box::new(BoxShape::new(Vec3::ZERO, Vec3::new(3.0, 0.5, 3.0), Quat::IDENTITY)));
        let bi = w.add_body(Body::sphere(Vec3::new(0.0, 4.0, 0.0), 0.5));
        simulate(&mut w, 3.0);
        let body = w.bodies[bi];
        assert!((body.pos.y - 1.0).abs() < 0.08, "rests on box top, y={}", body.pos.y);
        assert!(body.grounded, "should be grounded on the box");
    }

    #[test]
    fn static_capsule_collider_stops_falling_sphere() {
        // A sphere falls onto a horizontal static capsule bar and rests on top.
        let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
        w.add_collider(Box::new(CapsuleShape { a: Vec3::new(-2.0, 0.0, 0.0), b: Vec3::new(2.0, 0.0, 0.0), radius: 0.5 }));
        let bi = w.add_body(Body::sphere(Vec3::new(0.0, 4.0, 0.0), 0.5));
        simulate(&mut w, 3.0);
        let body = w.bodies[bi];
        assert!((body.pos.y - 1.0).abs() < 0.08, "rests on capsule, y={}", body.pos.y);
    }

    #[test]
    fn raycast_hits_ground_and_mesh() {
        // Ray straight down from above a ground plane → hits near y=0 with an up normal.
        let mut w = PhysicsWorld::new(GravityField::default());
        w.add_collider(Box::new(Plane::ground(0.0)));
        let hit = w.raycast(Vec3::new(0.0, 5.0, 0.0), Vec3::new(0.0, -1.0, 0.0), 20.0).expect("hit ground");
        assert!(hit.point[1].abs() < 0.1, "ground y={}", hit.point[1]);
        assert!(hit.normal[1] > 0.9, "up normal");
        assert!((hit.distance - 5.0).abs() < 0.1, "dist={}", hit.distance);
        // A ray that points away from everything misses.
        assert!(w.raycast(Vec3::new(0.0, 5.0, 0.0), Vec3::new(0.0, 1.0, 0.0), 20.0).is_none());
        // Against a triangle-mesh floor too.
        let (v, i) = floor_quad(0.0, 6.0);
        let mut wm = PhysicsWorld::new(GravityField::default());
        wm.add_collider(Box::new(TriMeshCollider::new(&v, &i)));
        let h2 = wm.raycast(Vec3::new(0.0, 4.0, 0.0), Vec3::new(0.0, -1.0, 0.0), 20.0).expect("hit mesh");
        assert!(h2.point[1].abs() < 0.2, "mesh hit y={}", h2.point[1]);
    }

    #[test]
    fn body_without_gravity_floats() {
        let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
        let mut b = Body::sphere(Vec3::new(0.0, 5.0, 0.0), 0.5);
        b.use_gravity = false;
        let bi = w.add_body(b);
        simulate(&mut w, 2.0);
        assert!((w.bodies[bi].pos.y - 5.0).abs() < 1e-3, "should float, y={}", w.bodies[bi].pos.y);
    }

    #[test]
    fn empty_gravity_field_is_zero_g() {
        // No sources → no pull (a space/zero-g world).
        let mut w = PhysicsWorld::new(GravityField::default());
        let bi = w.add_body(Body::sphere(Vec3::new(0.0, 5.0, 0.0), 0.5));
        simulate(&mut w, 2.0);
        assert!((w.bodies[bi].pos.y - 5.0).abs() < 1e-3, "zero-g drift, y={}", w.bodies[bi].pos.y);
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

        let mut sim = Sim::build(&ecs, &[(DVec3::ZERO, &terrain)], GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)), DVec3::ZERO);
        for _ in 0..240 {
            sim.advance(&mut ecs, 1.0 / 60.0, None);
        }
        let y = ecs.get::<Transform>(e).unwrap().translation.y;
        assert!((y - 0.5).abs() < 0.15, "entity settled at y={y}, expected ~0.5");
    }

    #[test]
    fn step_tick_advances_in_exact_gameplay_ticks() {
        // The netcode-era driver (docs/netcode-design.md §3): step_tick(1/60) must run
        // exactly the substeps of one gameplay tick and be reproducible — two sims fed
        // the same ticks land bit-identically (same build/machine determinism).
        let build = || {
            let mut ecs = World::default();
            let e = ecs.spawn();
            ecs.insert(e, Transform::from_translation(DVec3::new(0.0, 5.0, 0.0)));
            ecs.insert(e, RigidBody { radius: 0.5, ..Default::default() });
            let sim = Sim::build(
                &ecs,
                &[],
                GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)),
                DVec3::ZERO,
            );
            (ecs, e, sim)
        };
        let (mut a_ecs, ae, mut a) = build();
        let (mut b_ecs, be, mut b) = build();
        for _ in 0..60 {
            a.step_tick(1.0 / 60.0, None);
            b.step_tick(1.0 / 60.0, None);
        }
        a.writeback_interpolated(&mut a_ecs, 0.0);
        b.writeback_interpolated(&mut b_ecs, 0.0);
        let ya = a_ecs.get::<Transform>(ae).unwrap().translation.y;
        let yb = b_ecs.get::<Transform>(be).unwrap().translation.y;
        assert!(ya < 4.0, "body must fall under step_tick, got y={ya}");
        assert_eq!(ya, yb, "same ticks must reproduce bit-identical state");
        // Interpolation endpoints: alpha=1 must equal the body's current position.
        a.writeback_interpolated(&mut a_ecs, 1.0);
        let end = a_ecs.get::<Transform>(ae).unwrap().translation;
        let body = a.body_snapshot(ae.index()).unwrap().pos;
        assert!((end.y - body.y).abs() < 1e-6, "alpha=1 must land on the tick-end pos");
    }

    #[test]
    fn body_snapshot_round_trips_absolute_world_state() {
        // Capture → mutate → restore must return the body to the captured state, in
        // ABSOLUTE world coordinates even with a far-out floating origin (rollback's
        // core contract, docs/netcode-design.md §6).
        let far = DVec3::new(1.0e6, 0.0, 1.0e6); // origin-relative sim, far from 0
        let mut ecs = World::default();
        let e = ecs.spawn();
        ecs.insert(e, Transform::from_translation(far + DVec3::new(0.0, 5.0, 0.0)));
        ecs.insert(e, RigidBody { radius: 0.5, ..Default::default() });
        let mut sim =
            Sim::build(&ecs, &[], GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)), far);
        for _ in 0..30 {
            sim.step_tick(1.0 / 60.0, None);
        }
        let snap = sim.body_snapshot(e.index()).expect("body must snapshot");
        assert!((snap.pos.x - far.x).abs() < 1.0, "snapshot pos must be absolute world");
        // Diverge, then roll back.
        for _ in 0..30 {
            sim.step_tick(1.0 / 60.0, None);
        }
        let diverged = sim.body_snapshot(e.index()).unwrap();
        assert_ne!(diverged, snap, "the body should have kept falling");
        sim.restore_body(e.index(), &snap);
        let restored = sim.body_snapshot(e.index()).unwrap();
        assert_eq!(restored, snap, "restore must return the exact captured state");
    }

    #[test]
    fn rigidbody_wins_over_collidable_so_it_still_falls() {
        // A node flagged BOTH RigidBody and Collidable is a DYNAMIC body — the RigidBody
        // wins, so build() makes it a body and it falls under gravity. (The editor skips
        // adding a static collider for it so its dynamic body doesn't fight a static shape.)
        // This is the canonical character setup: a player capsule with a Rigidbody + a
        // Collider must not freeze in the air.
        let mut ecs = World::default();
        let e = ecs.spawn();
        ecs.insert(e, Transform::from_translation(DVec3::new(0.0, 5.0, 0.0)));
        ecs.insert(e, RigidBody { radius: 0.5, ..Default::default() });
        ecs.insert(e, floptle_core::Collidable);

        let mut sim = Sim::build(&ecs, &[], GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)), DVec3::ZERO);
        assert_eq!(sim.world.bodies.len(), 1, "a RigidBody node must become a dynamic body");
        for _ in 0..120 {
            sim.advance(&mut ecs, 1.0 / 60.0, None);
        }
        let y = ecs.get::<Transform>(e).unwrap().translation.y;
        assert!(y < 4.0, "a RigidBody node must fall under gravity, got y={y}");
    }

    #[test]
    fn sync_dynamic_params_resizes_shape_without_resetting_motion() {
        // Editing a RigidBody's radius/kind live in the Inspector while playing must
        // resize/reshape the running body in place — not reset its position or velocity
        // (that would feel like a teleport/restart to the dev testing the change).
        let mut ecs = World::default();
        let e = ecs.spawn();
        ecs.insert(e, Transform::from_translation(DVec3::new(0.0, 5.0, 0.0)));
        ecs.insert(e, RigidBody { kind: BodyKind::Sphere, radius: 0.5, ..Default::default() });

        let mut sim = Sim::build(&ecs, &[], GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)), DVec3::ZERO);
        sim.world.bodies[0].vel = Vec3::new(1.0, -2.0, 0.0);
        let pos_before = sim.world.bodies[0].pos;

        // Grow the radius and switch to a capsule, as if dragged in the Inspector.
        if let Some(rb) = ecs.get_mut::<RigidBody>(e) {
            rb.kind = BodyKind::Capsule;
            rb.radius = 1.5;
            rb.height = 4.0;
        }
        sim.sync_dynamic_params(&ecs);

        let body = &sim.world.bodies[0];
        assert_eq!(body.radius, 1.5, "radius should update live");
        assert!(matches!(body.shape, BodyShape::Capsule { half_height } if (half_height - 0.5).abs() < 1e-5));
        assert_eq!(body.pos, pos_before, "resizing must not move the body");
        assert_eq!(body.vel, Vec3::new(1.0, -2.0, 0.0), "resizing must not touch velocity");
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

    #[test]
    fn writeback_interpolates_between_fixed_steps() {
        // THE moving-jitter fix: rendered motion must advance by exactly real_dt · v
        // every frame, even when a frame consumes a fractional number of fixed steps.
        // Without interpolation a 1.5-step frame renders 1 step (or 2), so on-screen
        // displacement alternates — the "player jerks back and forth" bug.
        let mut ecs = World::default();
        let e = ecs.spawn();
        ecs.insert(e, Transform::from_translation(DVec3::ZERO));
        ecs.insert(e, RigidBody { radius: 0.5, gravity: false, ..Default::default() });

        let mut sim = Sim::build(&ecs, &[], GravityField::default(), DVec3::ZERO);
        sim.world.bodies[0].vel = Vec3::new(2.0, 0.0, 0.0);
        let dt = sim.fixed_dt;

        // Frames of 1.5 steps each: rendered x must grow by exactly 1.5·dt·v per frame.
        let mut last = 0.0f64;
        for i in 1..=4 {
            sim.advance(&mut ecs, dt * 1.5, None);
            let x = ecs.get::<Transform>(e).unwrap().translation.x;
            let step = x - last;
            let want = (1.5 * dt * 2.0) as f64;
            // The very first frame renders one whole step less (interpolation trails
            // real time by a constant fixed step — 8 ms of latency, not a wobble).
            let expect = if i == 1 { want - (dt * 2.0) as f64 } else { want };
            assert!((step - expect).abs() < 1e-5, "frame {i}: moved {step}, expected {expect}");
            last = x;
        }
    }

    #[test]
    fn anchored_collider_is_exact_far_from_world_origin() {
        // ADR-0015 end-to-end: content placed 10 million units out must collide as
        // exactly as content at the origin. At 1e7 an f32 ulp is a full unit — baking
        // world-space f32 verts there (the old path) is off by up to a meter.
        let far = DVec3::new(1.0e7, 0.0, 1.0e7);
        let mut ecs = World::default();
        let e = ecs.spawn();
        ecs.insert(e, Transform::from_translation(far + DVec3::new(0.25, 5.0, 0.0)));
        ecs.insert(e, RigidBody { radius: 0.5, ..Default::default() });

        let origin = (far + DVec3::new(0.0, 5.0, 0.0)).round();
        let mut sim =
            Sim::build(&ecs, &[], GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)), origin);
        // A 10×1×10 platform whose top face is at world y = 0.5.
        sim.add_static_box(far, Vec3::new(5.0, 0.5, 5.0), Quat::IDENTITY, 0);
        for _ in 0..240 {
            sim.advance(&mut ecs, 1.0 / 60.0, None);
        }
        let t = ecs.get::<Transform>(e).unwrap().translation;
        assert!((t.y - 1.0).abs() < 0.02, "should rest on the platform top, y = {}", t.y);
        assert!((t.x - (far.x + 0.25)).abs() < 1e-3, "must not drift in x, x = {}", t.x);
    }

    #[test]
    fn rebase_is_invisible_in_world_space() {
        // Recentering the sim's frame must not move anything on screen: world-space
        // writeback positions stay put and colliders keep holding bodies.
        let mut ecs = World::default();
        let e = ecs.spawn();
        ecs.insert(e, Transform::from_translation(DVec3::new(0.3, 2.0, 0.0)));
        ecs.insert(e, RigidBody { radius: 0.5, ..Default::default() });

        let mut sim =
            Sim::build(&ecs, &[], GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)), DVec3::ZERO);
        sim.add_static_box(DVec3::ZERO, Vec3::new(5.0, 0.5, 5.0), Quat::IDENTITY, 0);
        for _ in 0..240 {
            sim.advance(&mut ecs, 1.0 / 60.0, None);
        }
        let before = ecs.get::<Transform>(e).unwrap().translation;
        assert!((before.y - 1.0).abs() < 0.02, "settled on the box first, y = {}", before.y);

        sim.world.rebase(DVec3::new(5000.0, 0.0, -3000.0));
        for _ in 0..120 {
            sim.advance(&mut ecs, 1.0 / 60.0, None);
        }
        let after = ecs.get::<Transform>(e).unwrap().translation;
        assert!((after - before).length() < 1e-3, "rebase moved the body: {before} -> {after}");
    }

    #[test]
    fn terrain_volume_is_exact_far_from_world_origin() {
        // The full terrain path at distance (ADR-0015): a node-local flat field anchored
        // ten million units out must catch a falling body exactly at its surface — the
        // world placement lives in the f64 anchor, the field's own numbers stay small.
        let far = DVec3::new(1.0e7, 0.0, 1.0e7);
        let terrain =
            Terrain::flat([16, 16, 16], [0.0, 0.0, 0.0], [8.0, 8.0, 8.0], 0.0, [0.4, 0.6, 0.3]);
        let mut ecs = World::default();
        let e = ecs.spawn();
        ecs.insert(e, Transform::from_translation(far + DVec3::new(0.25, 5.0, 0.0)));
        ecs.insert(e, RigidBody { radius: 0.5, ..Default::default() });

        let origin = (far + DVec3::new(0.0, 5.0, 0.0)).round();
        let mut sim = Sim::build(
            &ecs,
            &[(far, &terrain)],
            GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)),
            origin,
        );
        for _ in 0..240 {
            sim.advance(&mut ecs, 1.0 / 60.0, None);
        }
        let t = ecs.get::<Transform>(e).unwrap().translation;
        assert!((t.y - 0.5).abs() < 0.15, "should rest on the terrain surface, y = {}", t.y);
        assert!((t.x - (far.x + 0.25)).abs() < 1e-3, "must not drift in x, x = {}", t.x);
    }

    #[test]
    fn lock_from_start_freezes_at_spawn_not_zero() {
        // Lock Y on a body spawned at (5, 7, 3): it must STAY at y=7 while gravity
        // pulls — not snap to y=0 (locks restore `home`, which must be the spawn).
        let mut ecs = World::default();
        let e = ecs.spawn();
        ecs.insert(e, Transform::from_translation(DVec3::new(5.0, 7.0, 3.0)));
        ecs.insert(e, RigidBody { radius: 0.5, lock_pos: [false, true, false], ..Default::default() });

        let mut sim = Sim::build(&ecs, &[], GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)), DVec3::ZERO);
        for _ in 0..60 {
            sim.advance(&mut ecs, 1.0 / 60.0, None);
        }
        let t = ecs.get::<Transform>(e).unwrap().translation;
        assert!((t.y - 7.0).abs() < 1e-3, "locked Y should stay at 7, got {}", t.y);
    }

    #[test]
    fn lock_toggled_mid_play_freezes_in_place() {
        // A lock toggled DURING play (Inspector toggle or a script's `rig.lock_x =
        // true`, both land via sync_dynamic_params) freezes the body where it IS —
        // it must NOT teleport back to its spawn position.
        let mut ecs = World::default();
        let e = ecs.spawn();
        ecs.insert(e, Transform::from_translation(DVec3::new(5.0, 7.0, 3.0)));
        ecs.insert(e, RigidBody { radius: 0.5, gravity: false, ..Default::default() });

        let mut sim = Sim::build(&ecs, &[], GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)), DVec3::ZERO);
        sim.world.bodies[0].vel = Vec3::new(1.0, 0.0, 0.0);
        for _ in 0..30 {
            sim.advance(&mut ecs, 1.0 / 60.0, None);
        }
        let x_before = ecs.get::<Transform>(e).unwrap().translation.x; // ~5.5
        ecs.get_mut::<RigidBody>(e).unwrap().lock_pos[0] = true;
        sim.sync_dynamic_params(&ecs);
        for _ in 0..30 {
            sim.advance(&mut ecs, 1.0 / 60.0, None);
        }
        let t = ecs.get::<Transform>(e).unwrap().translation;
        assert!(
            (t.x - x_before).abs() < 0.05,
            "mid-play lock must freeze in place: was at x={x_before}, locked to x={}",
            t.x
        );
    }

    #[test]
    fn rot_lock_toggled_mid_play_keeps_current_rotation() {
        // Same for rotation: a script yaws the node during play, then locks rot Y —
        // the writeback must hold the CURRENT yaw, not snap back to the authored 0.
        let mut ecs = World::default();
        let e = ecs.spawn();
        ecs.insert(e, Transform::from_translation(DVec3::new(0.0, 5.0, 0.0)));
        ecs.insert(e, RigidBody { radius: 0.5, gravity: false, ..Default::default() });

        let mut sim = Sim::build(&ecs, &[], GravityField::default(), DVec3::ZERO);
        for _ in 0..5 {
            sim.advance(&mut ecs, 1.0 / 60.0, None);
        }
        // Play-time rotation (as a script would write), then the lock engages.
        ecs.get_mut::<Transform>(e).unwrap().rotation = Quat::from_rotation_y(1.0);
        ecs.get_mut::<RigidBody>(e).unwrap().lock_rot[1] = true;
        sim.sync_dynamic_params(&ecs);
        for _ in 0..5 {
            sim.advance(&mut ecs, 1.0 / 60.0, None);
        }
        let (yaw, _, _) = ecs.get::<Transform>(e).unwrap().rotation.to_euler(EulerRot::YXZ);
        assert!((yaw - 1.0).abs() < 1e-4, "locked yaw should hold at 1.0, got {yaw}");
    }
}
