//! What a craft costs to simulate grows with what it is near, not with the
//! size of the level: guards on the work per step, as ratios of two timings
//! taken in the same run.

use floptle_core::math::{DVec3, Vec3};
use floptle_core::{BodyKind, BodyMode, Parent, RigidBody, Transform, World};
use floptle_physics::{ChunkTerrain, CollisionShape, GravityField, Sim};

fn median_ns(t: &ChunkTerrain, points: &[Vec3]) -> f64 {
    let mut runs: Vec<f64> = (0..7)
        .map(|_| {
            let start = std::time::Instant::now();
            let mut sum = 0.0f32;
            for _ in 0..20 {
                for &p in points {
                    sum += t.distance(p);
                }
            }
            std::hint::black_box(sum);
            start.elapsed().as_nanos() as f64 / (20 * points.len()) as f64
        })
        .collect();
    runs.sort_by(f64::total_cmp);
    runs[3]
}

/// Asked from a few metres up, the drawn-surface collider costs no more than
/// it does from a hand's width over the ground. The air reads exactly the
/// field's band, and a reject that wanted more than the band let every such
/// query run the full bucket search: a craft climbing off the pad cost 40 to
/// 60 ms of physics a frame.
#[test]
fn a_query_in_open_air_costs_no_more_than_one_at_the_ground() {
    let mut field = floptle_field::ChunkField::new(0.75);
    field.fill_slab(Vec3::new(-40.0, -6.0, -40.0), Vec3::new(40.0, 0.0, 40.0), 0.0, [0.5; 3]);
    let mut t = ChunkTerrain::new(field);
    t.mesh_accurate = true;
    let at = |h: f32| -> Vec<Vec3> {
        (0..200).map(|k| Vec3::new((k % 20) as f32 * 1.3 - 12.0, h, (k / 20) as f32 * 1.7 - 8.0)).collect()
    };
    let (ground, air) = (at(0.3), at(5.0));
    // Mesh what the ground queries read, so neither side pays for meshing.
    median_ns(&t, &ground);
    median_ns(&t, &air);
    assert!(air.iter().all(|&p| t.distance(p) >= 3.0 - 1e-3), "the air points should be past the band");
    let (g, a) = (median_ns(&t, &ground), median_ns(&t, &air));
    assert!(a < g * 1.5, "a query in open air costs {a:.0} ns, at the ground {g:.0} ns");
}

/// Seconds a 4-part craft takes to fly 120 ticks over a floor, with `far`
/// small static colliders standing well away from it.
fn craft_cost(far: usize) -> f64 {
    let mut ecs = World::default();
    let floor = ecs.spawn();
    ecs.insert(floor, Transform::from_translation(DVec3::new(0.0, -1.0, 0.0)));
    ecs.insert(floor, RigidBody { kind: BodyKind::Box, mode: BodyMode::Static, half_extents: [50.0, 1.0, 50.0], ..Default::default() });
    for k in 0..far {
        let e = ecs.spawn();
        let at = DVec3::new(500.0 + (k % 20) as f64 * 6.0, 0.0, 500.0 + (k / 20) as f64 * 6.0);
        ecs.insert(e, Transform::from_translation(at));
        ecs.insert(e, RigidBody { kind: BodyKind::Box, mode: BodyMode::Static, half_extents: [1.0, 1.0, 1.0], ..Default::default() });
    }
    let root = ecs.spawn();
    ecs.insert(root, Transform::from_translation(DVec3::new(0.0, 1.5, 0.0)));
    ecs.insert(root, RigidBody { assembly: true, ..Default::default() });
    for k in 0..4 {
        let p = ecs.spawn();
        ecs.insert(p, Transform::from_translation(DVec3::new(0.0, k as f64 * 1.1, 0.0)));
        ecs.insert(p, Parent(root));
        ecs.insert(p, RigidBody { kind: BodyKind::Box, half_extents: [0.5, 0.5, 0.5], mass: 1.0, ..Default::default() });
    }
    let mut sim = Sim::build(&ecs, &[], GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)), DVec3::ZERO);
    // A climb: pushed up and along, so it never settles to sleep.
    let mut runs: Vec<f64> = (0..5)
        .map(|_| {
            let start = std::time::Instant::now();
            for _ in 0..120 {
                sim.set_compound_velocity(root.index(), Vec3::new(3.0, 1.0, 0.0));
                sim.step_tick(1.0 / 60.0, None);
            }
            start.elapsed().as_secs_f64()
        })
        .collect();
    runs.sort_by(f64::total_cmp);
    runs[2]
}

/// Four hundred colliders far from a flying craft cost it little: its
/// contact pass asks only the colliders it can reach. It used to ask every
/// collider for every sample, twice a substep, and the same craft cost over
/// 300 times as much. What is left (about 2.6×) is the per-step bookkeeping
/// that does walk the whole list.
#[test]
fn a_flying_craft_does_not_pay_for_colliders_far_from_it() {
    let (near, many) = (craft_cost(0), craft_cost(400));
    assert!(many < near * 8.0, "400 far colliders made the craft cost {:.2}x as much ({near:.5} s, {many:.5} s)", many / near);
}
