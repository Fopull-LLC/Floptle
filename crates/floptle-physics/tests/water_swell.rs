//! Water with a swell: the solver floats things on the moving surface, a
//! splash is where that surface is, and the same tick always sees the same
//! water.

use std::sync::Arc;

use floptle_core::math::{DVec3, Vec3};
use floptle_core::water::{Swell, Wave};
use floptle_core::{BodyKind, RigidBody, Transform, World};
use floptle_physics::{BodySnapshot, GravityField, GravitySource, Sim, VolumeSwell, WaterShape, WaterVolume};

/// A pool 400 m across and 40 m deep, its still surface at y = 0, carrying
/// `waves`, and one body.
fn pool_with(body: RigidBody, at: DVec3, waves: &[Wave]) -> (Sim, floptle_core::Entity) {
    let mut ecs = World::default();
    let e = ecs.spawn();
    ecs.insert(e, Transform::from_translation(at));
    ecs.insert(e, body);
    let gravity = GravityField { sources: vec![GravitySource::Uniform(Vec3::new(0.0, -9.81, 0.0))] };
    let mut sim = Sim::build(&ecs, &[], gravity, DVec3::ZERO);
    sim.world.water.volumes.push(WaterVolume {
        shape: WaterShape::Box { center: Vec3::new(0.0, -20.0, 0.0), half: Vec3::new(200.0, 20.0, 200.0), rot: Default::default() },
        density: 1000.0,
        drag: 1.0,
        angular_drag: 1.0,
        frozen: false,
        entity: 7,
        carry: Default::default(),
        swell: Swell::new(waves, DVec3::Y).map(|swell| VolumeSwell { swell: Arc::new(swell), rot: Default::default(), time: 0.0 }),
    });
    (sim, e)
}

/// A crate that floats half under: its bounding sphere displaces twice its
/// weight of water.
fn crate_body() -> RigidBody {
    let r = Vec3::splat(0.5).length();
    let volume = 4.0 / 3.0 * std::f32::consts::PI * r * r * r;
    RigidBody { kind: BodyKind::Box, half_extents: [0.5, 0.5, 0.5], mass: 500.0 * volume, rotates: true, ..Default::default() }
}

const DT: f32 = 1.0 / 60.0;

/// One long roller: 1 m high, 60 m crest to crest, a rolling sine so the
/// surface over a point is the formula with no solving.
fn roller() -> Wave {
    Wave { amplitude: 1.0, wavelength: 60.0, direction: [1.0, 0.0], steepness: 0.0, speed: None, phase: 0.0 }
}

/// **A box rides the swell.** Floating in a 1 m roller it rises and falls
/// with the surface under it: it stays within 0.15 m of the analytic surface
/// the whole time, and its bob is in step with the wave, crest for crest.
#[test]
fn a_box_floats_at_the_surface_as_a_crest_passes() {
    let (mut sim, b) = pool_with(crate_body(), DVec3::new(0.0, 0.0, 0.0), &[roller()]);
    let k = std::f64::consts::TAU / 60.0;
    let omega = (9.81 * k).sqrt();
    let period = std::f64::consts::TAU / omega;
    let state = |sim: &Sim| sim.body_states().find(|s| s.entity == b).unwrap();
    // Let it settle onto the water, then watch three waves pass.
    for _ in 0..300 {
        sim.step_tick(DT, None);
    }
    let (mut worst, mut n) = (0.0f64, 0);
    // The bob against the wave: how much of it is in step, how much a
    // quarter turn late.
    let (mut in_step, mut late, mut power) = (0.0, 0.0, 0.0);
    let (mut lo, mut hi) = (f64::MAX, f64::MIN);
    let ticks = (3.0 * period / DT as f64) as usize;
    for _ in 0..ticks {
        sim.step_tick(DT, None);
        let (p, t) = (state(&sim).pos, sim.water_time());
        let theta = k * p.x - omega * t;
        let surface = theta.sin();
        worst = worst.max((p.y - surface).abs());
        in_step += p.y * theta.sin();
        late += p.y * -theta.cos();
        power += theta.sin() * theta.sin();
        (lo, hi) = (lo.min(p.y), hi.max(p.y));
        n += 1;
    }
    assert!(n > 900, "the fixture: three periods of a {period:.2} s wave");
    assert!(hi > 0.8 && lo < -0.8, "the box barely moved ({lo:.2} to {hi:.2} m) under a 1 m wave");
    assert!(worst < 0.15, "the box was {worst:.2} m off the surface it floats on");
    let (gain, lag) = (in_step / power, (late / in_step).atan());
    assert!((gain - 1.0).abs() < 0.1, "the box bobs {gain:.2} times as high as the wave");
    assert!(lag.abs() < 0.1, "the box bobs {lag:.2} rad out of step with the wave");
}

/// **A crate on a short chop stays in the water it is drawn in.** Waves
/// near a float's own bobbing rate would set it ringing, a metre clear of
/// the surface and a metre under it by turns. The water carries it instead:
/// in a steep three-wave sea a crate stays within 0.4 m of the surface over
/// it, where its waves stand 1.4 m either way.
#[test]
fn a_crate_on_a_short_chop_stays_on_the_surface() {
    let waves = [
        Wave { amplitude: 0.9, wavelength: 24.0, direction: [1.0, 0.2], steepness: 0.8, speed: None, phase: 0.0 },
        Wave { amplitude: 0.35, wavelength: 9.0, direction: [0.3, 1.0], steepness: 0.7, speed: None, phase: 1.3 },
        Wave { amplitude: 0.15, wavelength: 4.5, direction: [-0.6, 1.0], steepness: 0.6, speed: None, phase: 4.0 },
    ];
    let (mut sim, b) = pool_with(crate_body(), DVec3::new(-7.0, 0.3, 2.0), &waves);
    let (mut worst, mut tallest) = (0.0f32, 0.0f32);
    for i in 0..1200 {
        sim.step_tick(DT, None);
        if i < 180 {
            continue; // let it land
        }
        let p = sim.body_states().find(|s| s.entity == b).unwrap().pos.as_vec3();
        // The centre floats at the surface: under it by its depth, or above.
        let off = if sim.world.water.depth_at(p) > 0.0 { sim.world.water.depth_at(p) } else { sim.world.water.height_above(p) };
        worst = worst.max(off);
        tallest = tallest.max((p.y).abs());
    }
    assert!(tallest > 0.8, "the fixture: the crate rode waves only {tallest:.2} m high");
    assert!(worst < 0.4, "the crate was {worst:.2} m from the surface it floats on");
}

/// **A splash is at the surface it broke.** A wave held still (speed 0)
/// stands a metre high at its crest and a metre low in its trough: a ball
/// dropped onto each splashes there, not at the still level between.
#[test]
fn a_splash_is_reported_at_the_displaced_surface() {
    let still = Wave { speed: Some(0.0), ..roller() };
    let drop_at = |x: f64| {
        let ball = RigidBody { radius: 0.1, mass: 2.0, ..Default::default() };
        let (mut sim, _) = pool_with(ball, DVec3::new(x, 4.0, 0.0), &[still]);
        for _ in 0..240 {
            sim.step_tick(DT, None);
            if let Some(s) = sim.take_splashes().into_iter().find(|s| s.entering) {
                return s.pos;
            }
        }
        panic!("the ball never reached the water");
    };
    // sin(k·x) is 1 at a quarter wavelength and -1 at three quarters.
    let (crest, trough) = (drop_at(15.0), drop_at(45.0));
    assert!((crest.y - 1.0).abs() < 0.05, "on the crest the splash is at y = {:.3}, the surface is at 1", crest.y);
    assert!((trough.y + 1.0).abs() < 0.05, "in the trough the splash is at y = {:.3}, the surface is at -1", trough.y);
}

/// **The same tick sees the same water.** A host that counts ticks sets
/// the clock before each one. Rewind a body to where it was forty ticks
/// ago and run those ticks again at their own times, as a rollback does,
/// and it ends where it ended the first time, bit for bit.
#[test]
fn a_replayed_tick_floats_on_the_same_surface() {
    let waves: Vec<Wave> = (0..8)
        .map(|i| {
            let a = 0.4 + i as f32 * 0.83;
            Wave { amplitude: 0.6 / (1.0 + i as f32), wavelength: 50.0 / (1.0 + i as f32 * 0.7), direction: [a.cos(), a.sin()], steepness: 0.7, speed: None, phase: i as f32 }
        })
        .collect();
    let (mut sim, b) = pool_with(crate_body(), DVec3::new(3.0, 0.5, -2.0), &waves);
    let tick = |sim: &mut Sim, i: u32| {
        sim.set_water_time(i as f64 * DT as f64);
        sim.step_tick(DT, None);
    };
    let mut at_60: Option<BodySnapshot> = None;
    for i in 0..100 {
        if i == 60 {
            at_60 = sim.body_snapshot(b.index());
        }
        tick(&mut sim, i);
    }
    let first = sim.body_snapshot(b.index()).unwrap();
    assert!((first.pos - at_60.unwrap().pos).length() > 0.05, "the fixture: the box moved in those forty ticks");
    sim.restore_body(b.index(), &at_60.unwrap());
    for i in 60..100 {
        tick(&mut sim, i);
    }
    let again = sim.body_snapshot(b.index()).unwrap();
    assert_eq!((first.pos, first.vel), (again.pos, again.vel), "the replay floated on other water");
}

/// **Still water has no clock.** A pool with no waves floats a box exactly
/// as it did before swells existed, whatever the clock says, and a list of
/// no waves is no swell.
#[test]
fn still_water_is_untouched_by_the_clock() {
    let run = |poke: bool| {
        let (mut sim, b) = pool_with(crate_body(), DVec3::new(0.0, 1.0, 0.0), &[]);
        assert!(sim.world.water.volumes[0].swell.is_none() && !sim.world.water.has_swell());
        for i in 0..240 {
            if poke {
                sim.set_water_time(1.0e6 + i as f64 * 37.0);
            }
            sim.step_tick(DT, None);
        }
        let s = sim.body_snapshot(b.index()).unwrap();
        (s.pos, s.vel)
    };
    assert_eq!(run(false), run(true));
    // And the still surface is where it was: the pool's top face.
    let (sim, _) = pool_with(crate_body(), DVec3::ZERO, &[]);
    assert_eq!(sim.world.water.depth_at(Vec3::new(3.0, -1.25, 9.0)), 1.25);
    assert_eq!(sim.world.water.height_above(Vec3::new(3.0, 2.5, 9.0)), 2.5);
}

/// **A sea's swell turns with its world.** The same waves on a sphere the
/// solver holds, asked at a point over its axis and again with the sea's
/// node turned a quarter round: the surface that was over the first point
/// is over where the turn carried it.
#[test]
fn a_seas_swell_turns_with_its_node() {
    use floptle_core::math::Quat;
    let swell = Arc::new(Swell::new(&[roller()], DVec3::Y).unwrap());
    let sea = |rot: Quat| WaterVolume {
        shape: WaterShape::Sphere { center: Vec3::ZERO, radius: 800.0 },
        density: 1000.0,
        drag: 1.0,
        angular_drag: 1.0,
        frozen: false,
        entity: 3,
        carry: Default::default(),
        swell: Some(VolumeSwell { swell: swell.clone(), rot, time: 4.0 }),
    };
    // 15 m along the wave from the axis, two metres under the still level.
    let a = 15.0f32 / 800.0;
    let p = Vec3::new(a.sin(), a.cos(), 0.0) * 798.0;
    let turn = Quat::from_rotation_z(-std::f32::consts::FRAC_PI_2);
    let (still, turned) = (sea(Quat::IDENTITY).depth_at(p), sea(turn).depth_at(turn * p));
    let k = std::f64::consts::TAU / 60.0;
    let want = 2.0 + (k * 15.0 - (9.81 * k).sqrt() * 4.0).sin();
    assert!((still as f64 - want).abs() < 2e-3, "depth {still}, the formula says {want}");
    assert!((still - turned).abs() < 2e-3, "{still} before the turn, {turned} after");
    assert!((sea(Quat::IDENTITY).depth_at(turn * p) - still).abs() > 0.2, "the fixture: the turn moves the point across the wave");
}
