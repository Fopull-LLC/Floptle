//! A fast assembly hitting terrain comes off it no faster than its
//! restitution allows: it stops at first touch instead of arriving deep
//! inside the ground and being thrown back out.

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::{BodyKind, Parent, RigidBody, Transform, World};
use floptle_field::ChunkField;
use floptle_physics::{GravityField, Sim, TerrainVolume};

/// A 0.1 mass, restitution 0.05 one-part assembly (a 0.55-scale unit box)
/// at `y`, falling at `speed` onto a terrain slab whose top is y = 0.
/// Returns its peak height and peak upward speed after it first touched.
fn slam(speed: f32, y: f64) -> (f32, f32, f32) {
    let mut field = ChunkField::new(0.75);
    field.fill_slab(Vec3::new(-96.0, -6.0, -96.0), Vec3::new(96.0, 0.0, 96.0), 0.0, [0.5; 3]);
    let mut ecs = World::default();
    let root = ecs.spawn();
    ecs.insert(root, Transform::from_translation(DVec3::new(0.3, y, -0.2)));
    ecs.insert(root, RigidBody { assembly: true, restitution: 0.05, ..Default::default() });
    let part = ecs.spawn();
    ecs.insert(part, Transform { translation: DVec3::ZERO, rotation: Quat::from_rotation_x(0.3), scale: Vec3::splat(0.55) });
    ecs.insert(part, Parent(root));
    ecs.insert(part, RigidBody { kind: BodyKind::Box, half_extents: [0.5, 0.5, 0.5], mass: 0.1, ..Default::default() });
    let mut sim = Sim::build_layered(
        &ecs,
        &[TerrainVolume::new(DVec3::ZERO, &field)],
        GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)),
        DVec3::ZERO,
        floptle_core::Layers::default(),
    );
    sim.set_compound_velocity(root.index(), Vec3::new(0.0, -speed, 0.0));
    let mut touched = false;
    let (mut peak_y, mut peak_up, mut lowest) = (f32::NEG_INFINITY, 0.0f32, f32::INFINITY);
    for _ in 0..240 {
        sim.step_tick(1.0 / 60.0, None);
        let c = sim.compound_of(root.index()).unwrap();
        assert!(c.pos.is_finite() && c.vel.is_finite(), "state went non-finite");
        lowest = lowest.min(c.pos.y);
        if !touched && c.vel.y > -speed * 0.5 {
            touched = true;
        }
        if touched {
            peak_y = peak_y.max(c.pos.y);
            peak_up = peak_up.max(c.vel.y);
        }
    }
    assert!(touched, "it never reached the ground");
    (peak_y, peak_up, lowest)
}

/// Dropped at 140 m/s, it comes off at no more than 10 m/s and never climbs
/// more than 2 m off the surface; it never ends up deep inside the slab.
#[test]
fn a_fast_assembly_hitting_terrain_rebounds_within_its_restitution() {
    for (speed, y) in [(140.0, 3.0), (140.0, 2.37), (141.0, 6.11), (90.0, 4.4)] {
        let (peak_y, peak_up, lowest) = slam(speed, y);
        assert!(peak_up <= 10.0, "{speed} m/s from y={y}: came off at {peak_up} m/s");
        assert!(peak_y <= 2.0, "{speed} m/s from y={y}: climbed to {peak_y} m");
        assert!(lowest > -1.0, "{speed} m/s from y={y}: went {lowest} m into the slab");
    }
}

/// An assembly of `parts` (each `(offset along up, half extents, mass)`)
/// launched straight down at `speed` onto a generated planet that is moving
/// on its rails: every tick the planet's collider moves on, and the craft is
/// carried with it, as the editor's rails do. Returns the fastest it ever
/// moved outward after touching, and how far its centre of mass sits outside
/// the ground at the end (negative is inside it).
fn planet_slam(speed: f32, parts: &[(f32, [f32; 3], f32)]) -> (f32, f32) {
    use floptle_physics::GravitySource;
    let radius = 150.0;
    let field = floptle_field::procgen::generate_planet(&floptle_field::procgen::PlanetFill {
        seed: 11,
        radius,
        voxel: 1.5,
        relief: radius * 0.065,
        cave_depth: radius * 0.35,
        core_r: 10.5,
        ..Default::default()
    });
    let dir = Vec3::new(0.3, 1.0, 0.2).normalize();
    let ground_r = field.raycast(dir * radius * 1.5, -dir, radius).expect("ground").length();
    // Far from the world origin, as a planet in a solar system is.
    let mut anchor = DVec3::new(26518.0, 300.0, -4000.0);
    let mut ecs = World::default();
    let root = ecs.spawn();
    let up = Quat::from_rotation_arc(Vec3::Y, dir);
    let at = anchor + (dir * (ground_r + 25.0)).as_dvec3();
    ecs.insert(root, Transform { translation: at, rotation: up, scale: Vec3::ONE });
    ecs.insert(root, RigidBody { assembly: true, restitution: 0.05, friction: 0.7, ..Default::default() });
    for &(h, half, mass) in parts {
        let part = ecs.spawn();
        ecs.insert(part, Transform::from_translation(DVec3::new(0.0, h as f64, 0.0)));
        ecs.insert(part, Parent(root));
        ecs.insert(part, RigidBody { kind: BodyKind::Box, half_extents: half, mass, restitution: 0.05, friction: 0.6, ..Default::default() });
    }
    let mu = 7.4 * radius * radius;
    let gravity = |anchor: DVec3, origin: DVec3| floptle_physics::GravityField {
        sources: vec![GravitySource::InvSq { center: (anchor - origin).as_vec3(), mu, soi: 0.0, body_r: radius }],
    };
    let origin = (anchor + (dir * ground_r).as_dvec3()).round();
    let mut tv = TerrainVolume::new(anchor, &field);
    tv.eid = Some(9999);
    let mut sim = Sim::build_layered(&ecs, &[tv], gravity(anchor, origin), origin, floptle_core::Layers::default());
    sim.set_compound_velocity(root.index(), -dir * speed);
    // The planet's own motion on its rails, a tick at a time.
    let rails = DVec3::new(0.0, 0.0, 90.0) / 60.0;
    let (mut touched, mut peak_out) = (false, f32::NEG_INFINITY);
    for _ in 0..240 {
        anchor += rails;
        sim.set_collider_anchor(9999, anchor);
        sim.shift_compound(root.index(), rails);
        sim.world.gravity = gravity(anchor, sim.world.origin);
        sim.step_tick(1.0 / 60.0, Some(anchor + (dir * ground_r).as_dvec3()));
        let c = sim.compound_of(root.index()).unwrap();
        assert!(c.pos.is_finite() && c.vel.is_finite(), "state went non-finite");
        let out = c.vel.dot((c.pos.as_dvec3() + sim.world.origin - anchor).as_vec3().normalize());
        if !touched && out > -speed * 0.5 {
            touched = true;
        }
        if touched {
            peak_out = peak_out.max(out);
        }
    }
    assert!(touched, "it never reached the ground");
    let c = sim.compound_of(root.index()).unwrap();
    (peak_out, sim.world.colliders[0].distance(c.pos))
}

/// On a generated planet moving on its rails, a light one-part craft and a
/// three-part stack driven into the ground come off it no faster than their
/// restitution allows, and neither ends up inside it.
#[test]
fn a_fast_assembly_hitting_a_moving_planet_rebounds_within_its_restitution() {
    let chute: &[(f32, [f32; 3], f32)] = &[(0.0, [0.22, 0.17, 0.22], 0.08)];
    let stack: &[(f32, [f32; 3], f32)] =
        &[(0.0, [0.4, 0.4, 0.4], 0.5), (1.1, [0.5, 0.6, 0.5], 1.0), (2.2, [0.45, 0.4, 0.45], 0.45)];
    for (name, parts, speeds) in [("chute", chute, [10.0, 30.0, 70.0, 140.0]), ("stack", stack, [8.0, 20.0, 40.0, 60.0])] {
        for speed in speeds {
            let (out, clear) = planet_slam(speed, parts);
            // Restitution's share of the impact, plus what a tumbling corner
            // can add as it rolls off.
            assert!(out <= 0.05 * speed + 5.0, "{name} at {speed} m/s came off at {out} m/s");
            assert!(clear > -0.05, "{name} at {speed} m/s ended {clear} m inside the ground");
        }
    }
}
