//! A small craft lying on its side on terrain falls asleep and stays cheap
//! while a game keeps writing to it: a tank's mass burning off a few grams a
//! tick, parts re-posed where they already are. A change that could matter
//! to what it rests on still wakes it: a real load, a part that really
//! moved, a slow burn once it has added up.
//!
//! `examples/tipped_craft_bench.rs` prints the same craft's cost a tick, by
//! stage, case by case.

use floptle_core::math::{Quat, Vec3};
use floptle_physics::*;

const DT: f32 = 1.0 / 120.0;

fn ground(x: f32, z: f32) -> f32 {
    0.06 * x + 0.4 * (0.21 * x).sin() * (0.17 * z).cos()
}

/// The tank's shape id, and what it weighs full.
const TANK: u64 = 2;
const FULL: f32 = 12.0;

/// A bike authored upright along Z: a frame, a tank, a seat, two wheels and
/// a few thin parts.
fn bike() -> Vec<CompoundShape> {
    let part = |geom, offset: [f32; 3], rot, mass, id| CompoundShape { geom, offset: Vec3::from(offset), rot, mass, id };
    let b = |h: [f32; 3]| ShapeGeom::Box { half: Vec3::from(h) };
    let along_x = Quat::from_rotation_z(std::f32::consts::FRAC_PI_2);
    vec![
        part(b([0.12, 0.25, 0.8]), [0.0, 0.7, 0.0], Quat::IDENTITY, 90.0, 1),
        part(b([0.14, 0.15, 0.3]), [0.0, 1.0, 0.2], Quat::IDENTITY, FULL, TANK),
        part(b([0.15, 0.04, 0.35]), [0.0, 1.0, -0.4], Quat::IDENTITY, 4.0, 3),
        part(ShapeGeom::Capsule { radius: 0.33, half_height: 0.04 }, [0.0, 0.33, 0.85], along_x, 9.0, 4),
        part(ShapeGeom::Capsule { radius: 0.33, half_height: 0.04 }, [0.0, 0.33, -0.85], along_x, 9.0, 5),
        part(b([0.08, 0.012, 0.3]), [0.0, 0.7, 0.85], Quat::IDENTITY, 0.6, 6),
        part(ShapeGeom::Capsule { radius: 0.02, half_height: 0.35 }, [0.0, 1.25, 0.6], along_x, 1.0, 7),
        part(b([0.2, 0.015, 0.25]), [0.0, 0.95, -0.9], Quat::IDENTITY, 1.2, 8),
    ]
}

/// The bike lying on its side on rolling terrain, still and awake.
fn scene() -> (PhysicsWorld, usize) {
    let mut field = floptle_field::ChunkField::new(0.75);
    field.fill_with(Vec3::new(-14.0, -8.0, -14.0), Vec3::new(22.0, 8.0, 20.0), |p| (p.y - ground(p.x, p.z)) * 0.95, |_| [0.5; 3]);
    let mut w = PhysicsWorld::new(GravityField::uniform(Vec3::new(0.0, -9.81, 0.0)));
    w.set_colliders(vec![AnchoredCollider::world(Box::new(ChunkTerrain::new(field)))]);
    let (x, z) = (4.0, 3.0);
    let ci = w.add_compound(Compound::new(Vec3::new(x, ground(x, z) + 1.2, z), Quat::from_rotation_z(1.45), bike()));
    for _ in 0..360 {
        w.step(DT);
        w.compounds[ci].wake(WakeReason::Script);
    }
    assert!(w.compounds[ci].grounded, "the bike is not lying on the ground: the fixture proves nothing");
    (w, ci)
}

/// Step until it sleeps, at most `ticks`; the tick it did.
fn until_asleep(w: &mut PhysicsWorld, ci: usize, ticks: usize, mut each: impl FnMut(&mut PhysicsWorld, usize)) -> Option<usize> {
    for t in 0..ticks {
        each(w, t);
        w.step(DT);
        if w.compounds[ci].asleep {
            return Some(t);
        }
    }
    None
}

/// **A tank burning off two grams a tick lets its craft sleep**, and the
/// craft weighs exactly what its parts do all the while. Woken by every
/// change of mass, it never slept.
#[test]
fn a_tank_burning_off_a_little_a_tick_lets_its_craft_sleep() {
    let (mut w, ci) = scene();
    let burn = |w: &mut PhysicsWorld, t: usize| {
        w.compounds[ci].set_shape_masses(&[(TANK, FULL - 0.002 * (t + 1) as f32)]);
    };
    let slept = until_asleep(&mut w, ci, 240, burn);
    assert!(slept.is_some_and(|t| t <= 90), "a craft losing two grams a tick fell asleep at {slept:?}, not within the settle time");
    // And stays asleep while the burn goes on, short of what would matter.
    for t in 240..480 {
        burn(&mut w, t);
        w.step(DT);
        assert!(w.compounds[ci].asleep, "the burn woke it at tick {t} ({})", w.compounds[ci].wake.name());
    }
    let c = &w.compounds[ci];
    let parts: f32 = c.shapes.iter().map(|s| s.mass).sum();
    assert!((c.mass - parts).abs() < 1e-3, "the craft weighs {} and its parts {parts}", c.mass);
    let tank = c.shapes.iter().find(|s| s.id == TANK).unwrap().mass;
    assert!((tank - (FULL - 0.002 * 480.0)).abs() < 1e-4, "the tank's mass was not taken: {tank}");
}

/// **A slow burn wakes a sleeper once it adds up**: 20 g a tick is under
/// what matters for a long while, and over a twentieth of the craft's mass
/// after about three hundred ticks. Then it settles and sleeps again.
#[test]
fn a_slow_burn_wakes_a_sleeper_once_it_adds_up() {
    let (mut w, ci) = scene();
    assert!(until_asleep(&mut w, ci, 240, |_, _| {}).is_some(), "the bike never fell asleep");
    let mass0 = w.compounds[ci].mass;
    let mut woke = None;
    for t in 0..500 {
        w.compounds[ci].set_shape_masses(&[(TANK, FULL - 0.02 * (t + 1) as f32)]);
        if !w.compounds[ci].asleep {
            woke = Some(t);
            break;
        }
        w.step(DT);
    }
    let woke = woke.expect("a sleeper that lost most of its tank never woke");
    let lost = (mass0 - w.compounds[ci].mass) / mass0;
    assert_eq!(w.compounds[ci].wake, WakeReason::Reshaped);
    assert!(woke > 60, "it woke after {woke} ticks, {lost:.4} of its mass: a hair of fuel woke it");
    // By its mass or by its centre of mass, whichever gave first.
    assert!(lost > 0.01 && lost < MASS_GIVE * 1.1, "it woke having lost {lost:.4} of its mass");
    assert!(until_asleep(&mut w, ci, 240, |_, _| {}).is_some(), "woken by the burn, it never slept again");
}

/// **Loading a craft wakes it**: a change of mass that could tip it is not
/// slept through.
#[test]
fn a_real_change_of_mass_wakes_a_sleeper() {
    let (mut w, ci) = scene();
    assert!(until_asleep(&mut w, ci, 240, |_, _| {}).is_some(), "the bike never fell asleep");
    assert!(w.compounds[ci].set_shape_masses(&[(TANK, FULL * 5.0)]), "the mass was not taken");
    let c = &w.compounds[ci];
    assert!(!c.asleep, "a tank made five times heavier left the craft asleep");
    assert_eq!(c.wake, WakeReason::Reshaped);
}

/// The poses the craft's parts stand at now, as a game hands them back:
/// about the assembly origin.
fn poses(c: &Compound) -> Vec<(u64, Vec3, Quat)> {
    c.shapes.iter().map(|s| (s.id, s.offset - c.local_origin, s.rot)).collect()
}

/// **Parts re-posed where they already are do not wake the craft**, to
/// within the rounding a game's transforms carry; a part that really moved
/// does, and so does one creeping a tenth of a millimetre a tick, once the
/// creep adds up.
#[test]
fn re_posing_parts_wakes_the_craft_only_when_one_moved() {
    let (mut w, ci) = scene();
    let home = poses(&w.compounds[ci]);
    let slept = until_asleep(&mut w, ci, 240, |w, t| {
        // The same poses, a few microns off, differently each tick.
        let wobble = Vec3::splat(2e-6 * ((t % 5) as f32 - 2.0));
        let near: Vec<_> = home.iter().map(|&(id, at, rot)| (id, at + wobble, rot)).collect();
        assert!(!w.compounds[ci].resync_shape_geometry(&near), "re-posed where it was, it reported a wake at tick {t}");
    });
    assert!(slept.is_some_and(|t| t <= 90), "re-posed where it stood every tick, the craft fell asleep at {slept:?}");

    // A creep of 0.1 mm a tick: nothing at first, a wake within a few ticks
    // of the millimetre.
    let mut woke = None;
    for t in 1..40 {
        let mut crept = home.clone();
        crept[2].1.y += 1e-4 * t as f32;
        if w.compounds[ci].resync_shape_geometry(&crept) {
            woke = Some(t);
            break;
        }
    }
    assert!(woke.is_some_and(|t| (9..=13).contains(&t)), "a part creeping 0.1 mm a tick woke the craft at {woke:?}");
    assert_eq!(w.compounds[ci].wake, WakeReason::Reshaped);
    let seat = w.compounds[ci].shapes[2].offset - w.compounds[ci].local_origin;
    assert!((seat.y - (home[2].1.y + 1e-4 * woke.unwrap() as f32)).abs() < 1e-6, "the pose given was not taken");

    // A real move, and a real turn.
    assert!(until_asleep(&mut w, ci, 240, |_, _| {}).is_some(), "it never slept again");
    let mut folded = poses(&w.compounds[ci]);
    folded[5].1 += Vec3::new(0.0, 0.05, 0.0);
    assert!(w.compounds[ci].resync_shape_geometry(&folded), "a part moved 5 cm did not wake the craft");
    assert!(!w.compounds[ci].asleep);
    assert!(until_asleep(&mut w, ci, 240, |_, _| {}).is_some(), "it never slept again");
    let mut turned = poses(&w.compounds[ci]);
    turned[6].2 = (Quat::from_rotation_y(0.02) * turned[6].2).normalize();
    assert!(w.compounds[ci].resync_shape_geometry(&turned), "a part turned a degree did not wake the craft");
}

/// **A tipped craft burning fuel costs a sleeper's tick once it has
/// settled.** A ratio of two timings of the same run: its ticks from the
/// second second on against its first half second, awake. Kept awake by the
/// burn it read 1; asleep it reads a hundredth.
#[test]
fn a_tipped_craft_burning_fuel_costs_little_once_it_settles() {
    let (mut w, ci) = scene();
    w.time_steps(true);
    let run = |w: &mut PhysicsWorld, ticks: std::ops::Range<usize>| {
        let n = ticks.len() as f32;
        for t in ticks {
            w.compounds[ci].set_shape_masses(&[(TANK, FULL - 0.002 * (t + 1) as f32)]);
            w.step(DT);
        }
        let tm = w.take_timing().unwrap();
        (tm.static_ms + tm.sweeps_ms) / n
    };
    let awake = run(&mut w, 0..45);
    run(&mut w, 45..120);
    let settled = run(&mut w, 120..720);
    eprintln!("tipped craft burning fuel: {awake:.4} ms a tick awake, {settled:.4} once settled ({:.3}x)", settled / awake);
    assert!(
        settled < 0.25 * awake,
        "a tipped craft burning fuel costs {settled:.4} ms a tick long after it stopped moving, against {awake:.4} awake: \
         something keeps it awake"
    );
}
