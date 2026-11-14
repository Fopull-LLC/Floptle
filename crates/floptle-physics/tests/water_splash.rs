//! Things entering and leaving water: one event per crossing, and the slam.

use floptle_core::math::{DVec3, Vec3};
use floptle_core::{BodyKind, RigidBody, Transform, World};
use floptle_physics::{GravityField, GravitySource, Sim};

/// A pool 10 m across and 4 m deep, its surface at y = 0, and one body.
fn pool_with(body: RigidBody, at: DVec3, g: f32) -> (Sim, floptle_core::Entity) {
    let mut ecs = World::default();
    let e = ecs.spawn();
    ecs.insert(e, Transform::from_translation(at));
    ecs.insert(e, body);
    let gravity = GravityField { sources: vec![GravitySource::Uniform(Vec3::new(0.0, -g, 0.0))] };
    let mut sim = Sim::build(&ecs, &[], gravity, DVec3::ZERO);
    sim.world.water.volumes.push(floptle_physics::WaterVolume {
        shape: floptle_physics::WaterShape::Box { center: Vec3::new(0.0, -2.0, 0.0), half: Vec3::new(5.0, 2.0, 5.0), rot: Default::default() },
        density: 1000.0,
        drag: 1.0,
        angular_drag: 1.0,
        frozen: false,
        entity: 7,
        carry: Default::default(),
        swell: None,
    });
    (sim, e)
}

/// **A ball dropped into a pool splashes once, at the speed it hit.** A
/// 1 kg ball dropped from 5 m: one entering event, at √(2gh) within 5 %,
/// at the surface, with the water's up, its mass and its face; and none for
/// the bobbing after, which crosses slower than 0.5 m/s.
#[test]
fn a_ball_dropped_into_a_pool_splashes_once_at_the_speed_it_hit() {
    let r = (1.0f32 / (900.0 * 4.0 / 3.0 * std::f32::consts::PI)).cbrt();
    let (mut sim, ball) = pool_with(RigidBody { radius: r, mass: 1.0, ..Default::default() }, DVec3::new(0.0, 5.0 + r as f64, 0.0), 9.81);
    let mut events = Vec::new();
    for _ in 0..600 {
        sim.step_tick(1.0 / 60.0, None);
        events.extend(sim.take_splashes());
    }
    let entering: Vec<_> = events.iter().filter(|e| e.entering).collect();
    assert_eq!(entering.len(), 1, "{events:?}");
    let e = entering[0];
    let want = (2.0f32 * 9.81 * 5.0).sqrt();
    assert!((e.speed - want).abs() < 0.05 * want, "it hit at {} m/s, from 5 m that is {want}", e.speed);
    assert_eq!((e.node, e.water), (ball.index(), 7));
    assert!(e.pos.y.abs() < 1e-3 && (e.normal - Vec3::Y).length() < 1e-6, "{e:?}");
    assert!((e.mass - 1.0).abs() < 1e-6 && (e.area - std::f32::consts::PI * r * r).abs() < 1e-5);
    assert!(events.iter().all(|e| e.entering), "a ball that never left the water left it: {events:?}");
    assert_eq!(sim.dry_for(ball.index()), Some(0.0), "the ball is in the water");
}

/// **A ball thrown up out of the water says so once.** Just under the
/// surface and shot up at 12 m/s, it leaves (one exit, coming out at speed), and falls
/// back in (one entry).
#[test]
fn a_ball_leaving_the_water_fast_splashes_on_the_way_out() {
    let (mut sim, ball) = pool_with(RigidBody { radius: 0.2, mass: 30.0, ..Default::default() }, DVec3::new(0.0, -0.25, 0.0), 9.81);
    sim.set_body_velocity(ball.index(), Vec3::new(0.0, 12.0, 0.0));
    let mut events = Vec::new();
    for _ in 0..240 {
        sim.step_tick(1.0 / 60.0, None);
        events.extend(sim.take_splashes());
    }
    let out: Vec<_> = events.iter().filter(|e| !e.entering).collect();
    assert_eq!(out.len(), 1, "{events:?}");
    assert!(out[0].speed < -0.5, "it came out at {} m/s along -up", out[0].speed);
    assert!(sim.dry_for(ball.index()).is_some());
}

/// The speed a 2 t box hitting water at 40 m/s keeps after its first 0.1 s
/// in it, as a share of what it hit at, with the water's slam at `k`; and
/// whether it ever went back up in that time.
fn box_after_entry(k: f32) -> (f32, bool) {
    let body = RigidBody { kind: BodyKind::Box, half_extents: [1.0, 0.5, 1.0], mass: 2000.0, gravity: false, ..Default::default() };
    let (mut sim, b) = pool_with(body, DVec3::new(0.0, 2.0, 0.0), 0.0);
    if k > 0.0 {
        sim.set_water_slam([(7u32, k)].into_iter().collect());
    }
    sim.set_body_velocity(b.index(), Vec3::new(0.0, -40.0, 0.0));
    let vel = |sim: &Sim| sim.body_states().find(|s| s.entity == b).unwrap().vel;
    let (mut hit, mut wet_ticks, mut reversed) = (None, 0, false);
    for _ in 0..60 {
        let before = vel(&sim).y;
        sim.step_tick(1.0 / 60.0, None);
        if hit.is_none() && !sim.take_splashes().is_empty() {
            hit = Some(-before);
        }
        if let Some(hit) = hit {
            wet_ticks += 1;
            reversed |= vel(&sim).y > 0.0;
            if wet_ticks == 6 {
                return (-vel(&sim).y / hit, reversed);
            }
        }
    }
    panic!("the box never reached the water");
}

/// **A fast entry stops hard when the water slams.** With `setSlam` 1, a
/// 2 t box at 40 m/s loses 40 % of its speed or more in its first 0.1 s of
/// water, and is never thrown back; without it, the water's drag alone
/// takes much less.
#[test]
fn a_slam_stops_a_fast_entry_hard_and_never_reverses_it() {
    let (kept, reversed) = box_after_entry(1.0);
    assert!(kept <= 0.6, "it kept {:.0} % of its speed", kept * 100.0);
    assert!(!reversed, "the slam threw it back up");
    let (soft, _) = box_after_entry(0.0);
    assert!(soft > kept + 0.1, "without a slam it kept {:.0} %, with one {:.0} %", soft * 100.0, kept * 100.0);
}

/// **A slow crossing is bobbing, not a splash.** A ball drifting up out of
/// the water at 0.4 m/s leaves it with no event.
#[test]
fn a_slow_crossing_is_not_a_splash() {
    let (mut sim, ball) = pool_with(RigidBody { radius: 0.2, mass: 30.0, gravity: false, ..Default::default() }, DVec3::new(0.0, -0.1, 0.0), 0.0);
    sim.set_body_velocity(ball.index(), Vec3::new(0.0, 0.4, 0.0));
    let mut events = Vec::new();
    for _ in 0..180 {
        sim.step_tick(1.0 / 60.0, None);
        events.extend(sim.take_splashes());
    }
    let y = sim.body_states().find(|s| s.entity == ball).unwrap().pos.y;
    assert!(y > 0.25, "the fixture: the ball never left the water, it is at {y}");
    assert!(events.is_empty(), "{events:?}");
    assert!(sim.dry_for(ball.index()).is_some_and(|t| t > 0.0));
}
