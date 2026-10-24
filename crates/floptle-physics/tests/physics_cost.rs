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

/// A cube of `n` touching kinematic boxes (a builder's parts), and, with
/// `drop`, a dynamic box let fall onto it. Returns the scene, the sim and the
/// dropped box.
fn kinematic_stack(n: usize, drop: bool) -> (World, Sim, Option<floptle_core::Entity>) {
    let mut ecs = World::default();
    let side = (n as f64).cbrt().ceil() as usize;
    let mut k = 0;
    'outer: for x in 0..side {
        for y in 0..side {
            for z in 0..side {
                if k == n {
                    break 'outer;
                }
                let e = ecs.spawn();
                ecs.insert(e, Transform::from_translation(DVec3::new(x as f64, y as f64 + 0.5, z as f64)));
                ecs.insert(e, RigidBody { kind: BodyKind::Box, mode: BodyMode::Kinematic, half_extents: [0.5, 0.5, 0.5], ..Default::default() });
                k += 1;
            }
        }
    }
    let dropped = drop.then(|| {
        let e = ecs.spawn();
        ecs.insert(e, Transform::from_translation(DVec3::new(0.0, side as f64 + 2.0, 0.0)));
        ecs.insert(e, RigidBody { kind: BodyKind::Box, half_extents: [0.4, 0.4, 0.4], ..Default::default() });
        e
    });
    let sim = Sim::build(&ecs, &[], GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)), DVec3::ZERO);
    (ecs, sim, dropped)
}

/// The mean cost of a tick as the editor runs one: the per-frame sync (which
/// refreshes the kinematic hulls), then the step.
fn kinematic_tick(ecs: &World, sim: &mut Sim) -> f64 {
    let t = std::time::Instant::now();
    for _ in 0..5 {
        sim.sync_dynamic_params(ecs);
        sim.step_tick(1.0 / 60.0, None);
    }
    t.elapsed().as_secs_f64() / 5.0
}

/// A thousand touching kinematic parts that never move cost a tick in
/// proportion to ten of them, not to the square: two kinematic bodies are
/// never tested against each other. Each was tested against every other
/// every tick, 58 ms for a builder's craft. Timed in alternating windows,
/// each side's fastest kept, so a busy machine slows both alike; the bound
/// sits between the growth measured now (about 140x) and the old square
/// growth (about 7,500x).
#[test]
fn touching_kinematic_parts_cost_in_proportion() {
    let (small_ecs, mut small, _) = kinematic_stack(10, false);
    let (big_ecs, mut big, _) = kinematic_stack(1000, false);
    let (mut s, mut b) = (f64::MAX, f64::MAX);
    for _ in 0..8 {
        s = s.min(kinematic_tick(&small_ecs, &mut small));
        b = b.min(kinematic_tick(&big_ecs, &mut big));
    }
    assert!(b < s * 1000.0, "1000 parts cost {:.1}x what 10 do ({:.3} ms against {:.3} ms)", b / s, b * 1e3, s * 1e3);
}

/// A dynamic box dropped on a stack of kinematic parts still lands on it.
#[test]
fn a_box_dropped_on_kinematic_parts_lands_on_them() {
    let (ecs, mut sim, dropped) = kinematic_stack(1000, true);
    let e = dropped.unwrap();
    for _ in 0..180 {
        sim.sync_dynamic_params(&ecs);
        sim.step_tick(1.0 / 60.0, None);
    }
    let y = sim.body_states().find(|r| r.entity == e).unwrap().pos.y;
    // The stack's top is at y = 10; the box is 0.4 half-high.
    assert!((y - 10.4).abs() < 0.1, "the box came to rest at y = {y}, not on the stack");
}
