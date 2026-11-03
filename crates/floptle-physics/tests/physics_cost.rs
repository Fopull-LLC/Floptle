//! What a craft costs to simulate grows with what it is near, not with the
//! size of the level: guards on the work per step, as ratios of two timings
//! taken in the same run.

use floptle_core::math::{DVec3, Vec3};
use floptle_core::{BodyKind, BodyMode, Parent, RigidBody, Transform, World};
use floptle_physics::{ChunkTerrain, CollisionShape, GravityField, Sim};

/// Held while a timing runs, so two of this file's timings never measure
/// at once: `cargo test` runs a file's tests on threads side by side, and
/// one guard's work landed in another's measurement (2.5x read as 7.9x).
static TIMING: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn timing() -> std::sync::MutexGuard<'static, ()> {
    TIMING.lock().unwrap_or_else(|e| e.into_inner())
}

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
    let _one_at_a_time = timing();
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
    let _one_at_a_time = timing();
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

/// Seconds a 5-part craft takes to fly 120 ticks at 600 m/s, `height` metres
/// over a terrain slab, or with no terrain at all.
fn fast_craft_cost(terrain: bool, height: f64) -> f64 {
    craft_of_parts_cost(terrain, height, [0.5, 0.5, 0.5])
}

/// [`fast_craft_cost`] with parts of half extents `half`.
fn craft_of_parts_cost(terrain: bool, height: f64, half: [f32; 3]) -> f64 {
    craft_cost_on(terrain, height, half, 1.5, 5)
}

/// [`craft_of_parts_cost`] with `parts` parts, over terrain of `voxel` metres.
fn craft_cost_on(terrain: bool, height: f64, half: [f32; 3], voxel: f32, parts: usize) -> f64 {
    let mut field = floptle_field::ChunkField::new(voxel);
    field.fill_slab(Vec3::new(-200.0, -12.0, -200.0), Vec3::new(200.0, 0.0, 200.0), 0.0, [0.5; 3]);
    let mut ecs = World::default();
    let root = ecs.spawn();
    ecs.insert(root, Transform::from_translation(DVec3::new(0.0, height, 0.0)));
    ecs.insert(root, RigidBody { assembly: true, ..Default::default() });
    for k in 0..parts {
        let p = ecs.spawn();
        ecs.insert(p, Transform::from_translation(DVec3::new(0.0, height + k as f64 * (2.0 * half[1] as f64 + 0.1), 0.0)));
        ecs.insert(p, Parent(root));
        ecs.insert(p, RigidBody { kind: BodyKind::Box, half_extents: half, mass: 1.0, ..Default::default() });
    }
    let terrains: Vec<(DVec3, &floptle_field::ChunkField)> = if terrain { vec![(DVec3::ZERO, &field)] } else { Vec::new() };
    let mut sim = Sim::build(&ecs, &terrains, GravityField::uniform(Vec3::ZERO), DVec3::ZERO);
    let mut runs: Vec<f64> = (0..5)
        .map(|_| {
            let start = std::time::Instant::now();
            for _ in 0..120 {
                // Round and round over the slab, never leaving it.
                sim.set_compound_velocity(root.index(), Vec3::new(600.0, 0.0, 0.0));
                sim.step_tick(1.0 / 60.0, None);
                if sim.compound_of(root.index()).is_some_and(|c| c.pos.x > 150.0) {
                    sim.shift_compound(root.index(), DVec3::new(-300.0, 0.0, 0.0));
                }
            }
            start.elapsed().as_secs_f64()
        })
        .collect();
    runs.sort_by(f64::total_cmp);
    runs[2]
}

/// **A fast craft in open air does not pay for the terrain below it.** Each
/// tick a fast compound sweeps its samples along their move against what
/// they could meet, and a terrain answers open air with its band, a few
/// voxels, so every sample marched its whole move a band at a time: the same
/// craft cost ten times as much over a terrain as over nothing, at 30 m or
/// at 300 m, and a game's flown craft and falling stages paid it every tick.
/// A terrain whose chunks show the swept box is open air is left out of the
/// sweep now. What is left (about 2.5x) is the contact pass's own terrain
/// lookups, a field read a sample.
#[test]
fn a_fast_craft_in_open_air_does_not_pay_for_the_terrain_below() {
    let _one_at_a_time = timing();
    for h in [30.0, 300.0] {
        let (bare, over) = (fast_craft_cost(false, h), fast_craft_cost(true, h));
        eprintln!("at {h} m: bare {:.3} ms/tick, over terrain {:.3} ms/tick ({:.1}x)", bare * 1e3 / 120.0, over * 1e3 / 120.0, over / bare);
        assert!(over < bare * 5.0, "{h} m over a terrain the craft cost {:.1}x what it does over nothing", over / bare);
    }
}

/// Seconds an 8-part stage takes to fly 120 ticks at 600 m/s, 40 m over a
/// yard of `crates` static boxes (a base's walls and crates).
fn stage_over_crates_cost(crates: usize) -> f64 {
    let mut ecs = World::default();
    for k in 0..crates {
        let e = ecs.spawn();
        let at = DVec3::new((k % 16) as f64 * 20.0 - 160.0, 0.0, (k / 16) as f64 * 20.0 - 160.0);
        ecs.insert(e, Transform::from_translation(at));
        ecs.insert(e, RigidBody { kind: BodyKind::Box, mode: BodyMode::Static, half_extents: [2.0, 2.0, 2.0], ..Default::default() });
    }
    let root = ecs.spawn();
    ecs.insert(root, Transform::from_translation(DVec3::new(-150.0, 40.0, 0.0)));
    ecs.insert(root, RigidBody { assembly: true, ..Default::default() });
    for k in 0..8 {
        let p = ecs.spawn();
        ecs.insert(p, Transform::from_translation(DVec3::new(-150.0, 40.0 + k as f64 * 2.5, 0.0)));
        ecs.insert(p, Parent(root));
        ecs.insert(p, RigidBody { kind: BodyKind::Box, half_extents: [1.2, 1.2, 1.2], mass: 1.0, ..Default::default() });
    }
    let mut sim = Sim::build(&ecs, &[], GravityField::uniform(Vec3::ZERO), DVec3::ZERO);
    let mut runs: Vec<f64> = (0..5)
        .map(|_| {
            let start = std::time::Instant::now();
            for _ in 0..120 {
                sim.set_compound_velocity(root.index(), Vec3::new(600.0, 0.0, 0.0));
                sim.step_tick(1.0 / 60.0, None);
                if sim.compound_of(root.index()).is_some_and(|c| c.pos.x > 150.0) {
                    sim.shift_compound(root.index(), DVec3::new(-300.0, 0.0, 0.0));
                }
            }
            start.elapsed().as_secs_f64()
        })
        .collect();
    runs.sort_by(f64::total_cmp);
    runs[2]
}

/// **A fast stage pays only for the colliders it can reach.** A box, sphere
/// or capsule collider has a bounding sphere and no box, and the sweep of a
/// fast compound judged reach by the box alone: every such collider was a
/// candidate, so a stage of 8 parts falling over a base of 256 crates swept
/// each of its 72 samples against all 256 every tick, 11 ms a frame in a
/// game. Now the sphere answers: the same stage 40 m over 256 crates costs
/// what it does over none, within a few times.
#[test]
fn a_fast_stage_pays_only_for_the_colliders_it_can_reach() {
    let _one_at_a_time = timing();
    let (none, many) = (stage_over_crates_cost(0), stage_over_crates_cost(256));
    eprintln!("an 8-part stage at 600 m/s: {:.3} ms/tick over nothing, {:.3} over 256 crates ({:.1}x)", none * 1e3 / 120.0, many * 1e3 / 120.0, many / none);
    assert!(many < none * 4.0, "256 crates 40 m below made the stage cost {:.1}x as much", many / none);
}
