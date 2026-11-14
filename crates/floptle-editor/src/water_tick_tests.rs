//! Water that moves with what it lies on, tick by tick: a planet on its
//! rails, a node a script moves in `fixedUpdate`.

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_core::{CelestialBody, Matter, Name, RigidBody, ScriptInst, Scripts};

fn camera(ed: &mut crate::Editor, at: DVec3) -> floptle_core::Entity {
    let cam = ed.world.spawn();
    ed.world.insert(cam, Transform { translation: at, rotation: Quat::IDENTITY, scale: Vec3::ONE });
    ed.world.insert(cam, Name("Camera".into()));
    ed.world.insert(
        cam,
        Matter::Camera {
            fov_y: 1.0,
            active: true,
            target: String::new(),
            cull_mask: u32::MAX,
            target_w: 0,
            target_h: 0,
            target_hz: 0.0,
            ortho: false,
            ortho_height: 10.0,
            stack: Vec::new(),
            draws: floptle_core::CameraDraws::default(),
            post_after: false,
            near: 0.05,
            far: 300_000.0,
        },
    );
    cam
}

fn node(ed: &mut crate::Editor, name: &str, at: DVec3) -> floptle_core::Entity {
    let e = ed.world.spawn();
    ed.world.insert(e, Transform { translation: at, rotation: Quat::IDENTITY, scale: Vec3::ONE });
    ed.world.insert(e, Name(name.into()));
    ed.world.insert(e, Matter::Empty);
    e
}

/// A project directory holding one script, and an editor with a node
/// running it.
fn project(tag: &str, script: &str) -> (crate::Editor, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("floptle_water_tick_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    floptle_vfs::create_dir_all(dir.join("scripts")).unwrap();
    floptle_vfs::write(dir.join("scripts/driver.lua"), script).unwrap();
    let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
    let driver = node(&mut ed, "Driver", DVec3::ZERO);
    ed.world.insert(
        driver,
        Scripts(vec![ScriptInst {
            kind: "driver".into(),
            enabled: true,
            params: Vec::new(),
            refs: Vec::new(),
            strs: Vec::new(),
            pinned_bools: None,
        }]),
    );
    (ed, dir)
}

/// Where a 20 kg ball floats on a lake declared on a planet, as its distance
/// from the planet's centre, sampled every frame from the 5th second to the
/// 35th. The planet orbits at 280 m/s (4.7 m a tick) when `moving`, and
/// hardly moves otherwise; each frame runs `ticks` ticks; the camera (the
/// floating origin's focus, and the frame the sim rides) is on the planet or
/// 5 km off in the star's frame, where everything on the planet is carried
/// through the sim tick by tick.
fn floating_radii(moving: bool, ticks: usize, camera_on_planet: bool) -> Vec<f64> {
    let (mut ed, dir) = project(
        &format!("lake_{moving}_{ticks}_{camera_on_planet}"),
        r#"
local made = false
function update(node, dt)
  if not made then
    made = true
    water.addVolume{ shape = "lake", parent = "Planet", center = vec3(0, 0, 100), reach = 40 }
  end
end
"#,
    );
    let star = node(&mut ed, "Star", DVec3::new(0.0, -100_000.0, 0.0));
    ed.world.insert(star, CelestialBody { mu: if moving { 7.84e7 } else { 1.0 }, body_radius: 10.0, ..Default::default() });
    let planet = node(&mut ed, "Planet", DVec3::ZERO);
    ed.world.insert(
        planet,
        CelestialBody { mu: 9.8e4, body_radius: 95.0, soi: 2000.0, parent: "Star".into(), a: 1000.0, ..Default::default() },
    );
    let ball = node(&mut ed, "Ball", DVec3::ZERO);
    ed.world.insert(ball, RigidBody { radius: 0.5, mass: 20.0, ..Default::default() });
    let cam = camera(&mut ed, DVec3::ZERO);
    // One frame to find where the rails put the planet, then the scene laid
    // out about it.
    ed.toggle_play();
    ed.play_step(1.0 / 60.0, true);
    let p = floptle_core::world_transform(&ed.world, planet).translation;
    ed.toggle_play();
    ed.world.get_mut::<Transform>(planet).unwrap().translation = p;
    ed.world.get_mut::<Transform>(ball).unwrap().translation = p + DVec3::new(0.0, 0.0, 100.6);
    let cam_at = if camera_on_planet { p + DVec3::new(20.0, 0.0, 110.0) } else { p + DVec3::new(0.0, 5000.0, 0.0) };
    ed.world.get_mut::<Transform>(cam).unwrap().translation = cam_at;
    ed.toggle_play();
    let dt = ticks as f32 / 60.0;
    let frames = (35.0 / dt).round() as usize;
    let settle = (5.0 / dt).round() as usize;
    let mut out = Vec::new();
    for f in 0..frames {
        ed.play_step(dt, true);
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
        if f >= settle {
            let b = floptle_core::world_transform(&ed.world, ball).translation;
            let c = floptle_core::world_transform(&ed.world, planet).translation;
            out.push((b - c).length());
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    out
}

/// **A body floats on a lake on a fast planet exactly as on a still one.**
/// At 280 m/s the planet moves 4.7 m a tick; with the lake posed once a
/// frame, a frame of two ticks put the water a tick of travel behind the
/// planet for the second tick, and the ball was thrown out of it or sank in
/// it. Over 30 s, at one and two ticks a frame, riding the planet's frame or
/// carried through the star's, the ball's height above the planet's centre
/// stays within 5 cm of the same ball's on a still planet, sample for
/// sample: it bobs at the surface as it would anywhere.
#[test]
fn a_body_floats_on_a_lake_on_a_fast_planet_as_on_a_still_one() {
    for ticks in [1, 2] {
        let still = floating_radii(false, ticks, true);
        let mean = still.iter().sum::<f64>() / still.len() as f64;
        assert!((mean - 100.0).abs() < 0.6, "it floats {mean} m from the centre, the lake's surface is at 100");
        for on_planet in [true, false] {
            let r = floating_radii(true, ticks, on_planet);
            assert_eq!(r.len(), still.len());
            let worst = r.iter().zip(&still).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
            assert!(
                worst < 0.05,
                "{ticks} tick(s) a frame, camera {}: the ball strays {worst:.3} m from where it floats on a still planet",
                if on_planet { "on the planet" } else { "in the star's frame" }
            );
        }
    }
}

/// **Water on a node a script moves in `fixedUpdate` is where the node is
/// for that tick's physics and for `water.at`.** A pool lifted 5 m a tick,
/// two ticks a frame: the solver's pool and the one scripts ask about are
/// both at the node's tick position, not where the frame began.
#[test]
fn water_on_a_node_moved_every_tick_is_posed_every_tick() {
    let (mut ed, dir) = project(
        "lift",
        r#"
local n = 0
function fixedUpdate(node, dt)
  local tank = find("Tank")
  n = n + 1
  -- What the last tick's lift left: the pool's surface is 2 m above
  -- its node, so 1 m under the node's middle is 3 m down.
  log(string.format("D %d %.3f %.3f", n, tank.y, water.depthAt(tank.x, tank.y - 1, tank.z)))
  tank.y = tank.y + 5
end
"#,
    );
    let tank = node(&mut ed, "Tank", DVec3::ZERO);
    ed.world.insert(tank, Matter::default_water());
    camera(&mut ed, DVec3::new(0.0, 0.0, 30.0));
    ed.toggle_play();
    for _ in 0..10 {
        ed.play_step(2.0 / 60.0, true);
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
        let y = floptle_core::world_transform(&ed.world, tank).translation;
        let sim = ed.sim.as_ref().unwrap();
        let v = sim.world.water.volumes.iter().find(|v| v.entity == tank.index()).expect("the tank's water");
        let floptle_physics::WaterShape::Box { center, .. } = v.shape else { panic!("a pool is a box") };
        let at = sim.world.origin + center.as_dvec3();
        assert!((at - y).length() < 1e-3, "the solver's pool is at {at}, its node at {y}");
    }
    ed.drain_script_logs();
    let lines: Vec<String> = ed.console.entries.iter().filter_map(|e| e.msg.strip_prefix("D ").map(str::to_string)).collect();
    assert!(lines.len() >= 18, "{lines:?}");
    for l in &lines[1..] {
        let depth: f64 = l.split(' ').nth(2).unwrap().parse().unwrap();
        assert!((depth - 3.0).abs() < 1e-3, "water.depthAt in fixedUpdate read another tick's pool: {lines:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A pool on a moving ship carries what floats in it.** A tank of water
/// on a body coasting at 20 m/s, a ball floating in it: the water is posed
/// where the ship is each tick and moves with the ship, so the ball is
/// carried along in it rather than dragged back against water standing
/// still in the air, and after 5 s it is still in the tank.
#[test]
fn a_pool_on_a_moving_ship_carries_what_floats_in_it() {
    let (mut ed, dir) = project("deck", "function update(node, dt) end\n");
    let ship = node(&mut ed, "Ship", DVec3::ZERO);
    ed.world.insert(
        ship,
        RigidBody { kind: floptle_core::BodyKind::Box, half_extents: [6.0, 0.5, 6.0], mass: 1.0e6, gravity: false, ..Default::default() },
    );
    let tank = node(&mut ed, "Tank", DVec3::new(0.0, 3.0, 0.0));
    ed.world.insert(tank, Matter::default_water());
    ed.world.insert(tank, floptle_core::Parent(ship));
    let ball = node(&mut ed, "Ball", DVec3::new(0.0, 4.7, 0.0));
    ed.world.insert(ball, RigidBody { radius: 0.3, mass: 30.0, ..Default::default() });
    camera(&mut ed, DVec3::new(0.0, 5.0, 30.0));
    ed.toggle_play();
    ed.play_step(1.0 / 60.0, true);
    ed.sim.as_mut().unwrap().set_body_velocity(ship.index(), Vec3::new(20.0, 0.0, 0.0));
    for _ in 0..150 {
        ed.play_step(2.0 / 60.0, true);
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    }
    let (b, t) = (floptle_core::world_transform(&ed.world, ball).translation, floptle_core::world_transform(&ed.world, tank).translation);
    assert!(t.x > 90.0, "the ship never moved: the tank is at {t}");
    assert!((b.x - t.x).abs() < 3.0 && (b.z - t.z).abs() < 3.0, "the ball is at {b}, the tank at {t}: it was left behind");
    assert!(b.y > t.y, "the ball sank in the tank: {b} against {t}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A lake whose level a script moves every tick floats things at the new
/// level from the next tick**, with no rebuild: its id and the count of
/// volumes stay, the solver's lake has the radius the last tick set, and a
/// ball floating on it rises with it, 1.2 m in 2 s.
#[test]
fn a_lake_raised_every_tick_floats_a_body_at_its_new_level() {
    let (mut ed, dir) = project(
        "setlevel",
        r#"
local id, n = nil, 0
function update(node, dt)
  if not id then id = water.addVolume{ shape = "lake", parent = "Planet", center = vec3(0, 0, 100), reach = 40 } end
end
function fixedUpdate(node, dt)
  if not id then return end
  n = n + 1
  if n > 300 and n <= 420 then water.setLevel(id, 100 + (n - 300) * 0.01) end
  log(string.format("L %d %d %.4f", id, #water.volumes(), water.levelOf(id)))
end
"#,
    );
    let planet = node(&mut ed, "Planet", DVec3::ZERO);
    ed.world.insert(planet, CelestialBody { mu: 9.8e4, body_radius: 95.0, soi: 2000.0, ..Default::default() });
    let ball = node(&mut ed, "Ball", DVec3::new(0.0, 0.0, 100.4));
    ed.world.insert(ball, RigidBody { radius: 0.5, mass: 20.0, ..Default::default() });
    camera(&mut ed, DVec3::new(20.0, 0.0, 110.0));
    ed.toggle_play();
    let radius = |ed: &crate::Editor| {
        ed.sim.as_ref().unwrap().world.water.volumes.iter().find_map(|v| match &v.shape {
            floptle_physics::WaterShape::Declared { water, .. } => match water.surface {
                floptle_core::water::Surface::Lake { radius, .. } => Some(radius),
                _ => None,
            },
            _ => None,
        })
    };
    let mut rest = 0.0;
    for f in 0..240 {
        ed.play_step(2.0 / 60.0, true);
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
        let r = (floptle_core::world_transform(&ed.world, ball).translation).length();
        if f == 149 {
            rest = r;
        }
        // After a frame's ticks, the solver's lake is the level its last
        // tick set.
        let want = 100.0 + ((2 * (f + 1)) as f64 - 300.0).clamp(0.0, 120.0) * 0.01;
        if let Some(got) = radius(&ed) {
            assert!((got - want).abs() < 1e-9, "frame {f}: the solver's lake is at {got}, the script set {want}");
        }
    }
    let r = floptle_core::world_transform(&ed.world, ball).translation.length();
    assert!((r - rest - 1.2).abs() < 0.1, "the ball rose {} m with a lake raised 1.2 m", r - rest);
    ed.drain_script_logs();
    let ids: std::collections::HashSet<String> =
        ed.console.entries.iter().filter_map(|e| e.msg.strip_prefix("L ")).map(|l| l.split(' ').take(2).collect::<Vec<_>>().join(" ")).collect();
    assert_eq!(ids.len(), 1, "the lake's id or the count of volumes changed: {ids:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
