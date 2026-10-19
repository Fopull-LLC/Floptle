//! Assemblies as a script sees them: built from spawned parts, weighed and
//! pushed from Lua, and far from the camera in distant-craft LOD.

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_core::{BodyKind, CelestialBody, Matter, Name, RigidBody, ScriptInst, Scripts};

/// Write a one-node prefab: an Empty carrying `rb`.
fn prefab(dir: &std::path::Path, name: &str, rb: RigidBody) {
    let mut w = floptle_core::World::default();
    let e = w.spawn();
    w.insert(e, Transform::IDENTITY);
    w.insert(e, Name(name.into()));
    w.insert(e, Matter::Empty);
    w.insert(e, rb);
    let doc = floptle_scene::to_doc(name, &w);
    let text = ron::ser::to_string(&doc.nodes).unwrap();
    floptle_scene::portable::write(&dir.join(format!("prefabs/{name}{}", floptle_scene::PREFAB_EXT)), &text).unwrap();
}

/// A project with a `Vessel` assembly root and a `Part` box prefab, a camera
/// at the origin, a massless-ish star for the rails, and a driver node
/// running `script`.
fn project(tag: &str, script: &str) -> (crate::Editor, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("floptle_assembly_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    floptle_vfs::create_dir_all(dir.join("prefabs")).unwrap();
    floptle_vfs::create_dir_all(dir.join("scripts")).unwrap();
    prefab(&dir, "Vessel", RigidBody { assembly: true, gravity: false, ..Default::default() });
    prefab(
        &dir,
        "Part",
        RigidBody { kind: BodyKind::Box, half_extents: [0.5, 0.5, 0.5], mass: 2.0, gravity: false, ..Default::default() },
    );
    floptle_vfs::write(dir.join("scripts/driver.lua"), script).unwrap();

    let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
    let cam = ed.world.spawn();
    ed.world.insert(cam, Transform::IDENTITY);
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
        },
    );
    let star = ed.world.spawn();
    ed.world.insert(
        star,
        Transform { translation: DVec3::new(0.0, -100_000.0, 0.0), rotation: Quat::IDENTITY, scale: Vec3::ONE },
    );
    ed.world.insert(star, Name("Star".into()));
    ed.world.insert(star, Matter::Empty);
    ed.world.insert(star, CelestialBody { mu: 1.0, body_radius: 10.0, ..Default::default() });
    let driver = ed.world.spawn();
    ed.world.insert(driver, Transform::IDENTITY);
    ed.world.insert(driver, Name("Driver".into()));
    ed.world.insert(driver, Matter::Empty);
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

/// Every `TAG …` line the scripts have logged so far, the tag stripped.
fn lines(ed: &mut crate::Editor, tag: &str) -> Vec<String> {
    ed.drain_script_logs();
    let tag = format!("{tag} ");
    ed.console.entries.iter().filter_map(|e| e.msg.strip_prefix(&tag).map(str::to_string)).collect()
}

fn run(ed: &mut crate::Editor, frames: usize) {
    for _ in 0..frames {
        ed.play_step(1.0 / 60.0, true);
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    }
}

/// Spawned 2 km from the camera, built from four parts and pushed to
/// 100 m/s from `rebuild`'s callback in the same frame: the next frame it
/// is moving at 100 m/s, and while it coasts in LOD `assembly.info` says so
/// (free, not grounded, the rails velocity) rather than reporting the LOD's
/// anchored stand-in.
#[test]
fn a_far_assembly_pushed_as_it_is_built_moves_and_reads_true_in_lod() {
    let (mut ed, dir) = project(
        "lod",
        r#"
local asked, ship, frame = false, nil, 0
function update(node, dt)
  frame = frame + 1
  if not asked then
    asked = true
    spawn("Vessel", vec3(2000, 0, 0), function(v)
      ship = v
      local n = 0
      for i = 1, 4 do
        spawn("Part", vec3(2000 + i - 2.5, 0, 0), function(p)
          n = n + 1
          if n == 4 then
            assembly.rebuild(v, function(s)
              local inf = assembly.info(s)
              log(string.format("BUILT %.3f", inf.mass))
              assembly.impulseAt(s, vec3(100 * inf.mass, 0, 0), inf.com)
            end)
          end
        end, v)
      end
    end)
  end
  if ship then
    local inf = assembly.info(ship)
    if inf then
      log(string.format("INFO %.3f %s %s %d", inf.vel.x, tostring(inf.anchored), tostring(inf.grounded), frame))
    end
  end
end
"#,
    );
    ed.toggle_play();
    assert!(ed.playing);
    run(&mut ed, 4);
    assert_eq!(lines(&mut ed, "BUILT"), vec!["8.000".to_string()], "the callback sees the four-part compound");
    // Two seconds: a pushed craft stays live for a second after the push,
    // then goes to its rails.
    run(&mut ed, 120);
    let info = lines(&mut ed, "INFO");
    let after: Vec<&String> = info.iter().skip_while(|l| l.starts_with("0.000")).collect();
    assert!(after.len() >= 20, "the craft never moved: {info:?}");
    for l in &after {
        let mut it = l.split(' ');
        let v: f32 = it.next().unwrap().parse().unwrap();
        assert!((v - 100.0).abs() <= 1.0, "speed {v} (want 100 ± 1): {info:?}");
        assert_eq!(it.next(), Some("false"), "a coasting craft is not anchored: {info:?}");
        assert_eq!(it.next(), Some("false"), "a coasting craft is not grounded: {info:?}");
    }
    let root = ed.world.query::<Name>().find(|(_, n)| n.0 == "Vessel").map(|(e, _)| e.index()).unwrap();
    assert!(ed.compound_lod.contains_key(&root), "2 km out, the craft should be in LOD for this test to mean anything");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A part's mass written through its RigidBody handle is the assembly's
/// mass the next frame, with no rebuild, and `assembly.info` carries the
/// inertia the solver uses; a box part reads its scaled half-extents.
#[test]
fn a_part_mass_written_from_lua_reaches_the_assembly() {
    let (mut ed, dir) = project(
        "mass",
        r#"
local ship, parts, step = nil, {}, 0
function update(node, dt)
  step = step + 1
  if step == 1 then
    spawn("Vessel", vec3(0, 0, -10), function(v)
      ship = v
      for i = 1, 2 do
        spawn("Part", vec3(i * 2 - 3, 0, -10), function(p)
          parts[#parts + 1] = p
          if #parts == 2 then assembly.rebuild(v) end
        end, v)
      end
    end)
  elseif step == 5 then
    local rb = parts[1]:getcomponent("RigidBody")
    log(string.format("HALF %.3f", rb.worldHalfX))
    rb.mass = 5
  elseif step == 7 then
    local inf = assembly.info(ship)
    log(string.format("MASS %.3f %.4f", inf.mass, inf.inertia[2].y))
  end
end
"#,
    );
    ed.toggle_play();
    run(&mut ed, 8);
    assert_eq!(lines(&mut ed, "HALF"), vec!["0.500".to_string()]);
    let got = lines(&mut ed, "MASS");
    assert_eq!(got.len(), 1, "{got:?}");
    let mut it = got[0].split(' ');
    let mass: f32 = it.next().unwrap().parse().unwrap();
    let iyy: f32 = it.next().unwrap().parse().unwrap();
    assert!((mass - 7.0).abs() < 1e-3, "a 5 + 2 craft weighs {mass}");
    // Two unit cubes 2 m apart, 5 and 2: each m/6 about its own centre, plus
    // the parallel axis about the shared centre of mass.
    let (m1, m2, d) = (5.0f32, 2.0f32, 2.0f32);
    let want = m1 / 6.0 + m2 / 6.0 + m1 * m2 / (m1 + m2) * d * d;
    assert!((iyy - want).abs() < 1e-3, "Iyy {iyy}, want {want}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A four-part craft set down on a static slab sleeps after it settles, and
/// `node.asleep` on its root says so; writing it false wakes it, writing it
/// true sleeps it again, and an `assembly.force` wakes it.
#[test]
fn a_parked_assembly_sleeps_and_node_asleep_reads_and_writes_on_its_root() {
    let (mut ed, dir) = project(
        "sleep",
        r#"
local ship, built, step = nil, false, 0
function update(node, dt)
  step = step + 1
  if step == 1 then
    spawn("Vessel", vec3(0, 0.52, -10), function(v)
      ship = v
      v:getcomponent("RigidBody").gravity = true
      local n = 0
      for i = 1, 4 do
        local x, z = (i % 2) - 0.5, math.floor((i - 1) / 2) - 0.5
        spawn("Part", vec3(x, 0.52, -10 + z), function(p)
          n = n + 1
          if n == 4 then assembly.rebuild(v) end
        end, v)
      end
    end)
  elseif ship then
    if step == 200 then log("SLEEP settled " .. tostring(ship.asleep)) end
    if step == 201 then ship.asleep = false end
    if step == 202 then log("SLEEP woken " .. tostring(ship.asleep)) end
    if step == 203 then ship.asleep = true end
    if step == 205 then log("SLEEP slept " .. tostring(ship.asleep)) end
    if step == 206 then assembly.force(ship, vec3(500, 0, 0)) end
    if step == 208 then log("SLEEP pushed " .. tostring(ship.asleep)) end
  end
end
"#,
    );
    let ground = ed.world.spawn();
    ed.world.insert(
        ground,
        Transform { translation: DVec3::new(0.0, -1.0, -10.0), rotation: Quat::IDENTITY, scale: Vec3::ONE },
    );
    ed.world.insert(ground, Name("Ground".into()));
    ed.world.insert(ground, Matter::Empty);
    ed.world.insert(
        ground,
        RigidBody { kind: BodyKind::Box, mode: floptle_core::BodyMode::Static, half_extents: [10.0, 1.0, 10.0], ..Default::default() },
    );
    // Gravity this time, so it settles onto the slab.
    let g = ed.world.spawn();
    ed.world.insert(g, Transform::IDENTITY);
    ed.world.insert(g, Matter::GravityVolume { mode: floptle_core::GravityMode::Down, strength: 9.81, radius: 1000.0 });
    ed.toggle_play();
    run(&mut ed, 210);
    assert_eq!(
        lines(&mut ed, "SLEEP"),
        ["settled true", "woken false", "slept true", "pushed false"].map(String::from).to_vec()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A body switched off on a planet that is moving on its rails rides the
/// planet while it is off, and is where it was relative to the planet when
/// it is switched back on.
#[test]
fn a_switched_off_body_rides_its_planet_and_resumes_where_it_was_on_it() {
    let (mut ed, dir) = project(
        "offride",
        r#"
local step = 0
function update(node, dt)
  step = step + 1
  local ball, planet = find("Ball", { scope = "all" }), find("Planet")
  if not (ball and planet) then return end
  local rx, ry, rz = ball.x - planet.x, ball.y - planet.y, ball.z - planet.z
  if step == 30 then
    log(string.format("REL before %.2f %.2f %.2f %.2f %.2f %.2f", rx, ry, rz, planet.x, planet.y, planet.z))
    ball.enabled = false
  elseif step == 90 then
    ball.enabled = true
  elseif step == 93 then
    log(string.format("REL after %.2f %.2f %.2f %.2f %.2f %.2f", rx, ry, rz, planet.x, planet.y, planet.z))
  end
end
"#,
    );
    // A planet on a fast orbit about the star (100 m/s), and a ball that
    // does not fall, hanging above it inside its sphere of influence.
    let star = ed.world.query::<Name>().find(|(_, n)| n.0 == "Star").map(|(e, _)| e).unwrap();
    ed.world.get_mut::<CelestialBody>(star).unwrap().mu = 1.0e7;
    let planet = ed.world.spawn();
    ed.world.insert(planet, Transform::IDENTITY);
    ed.world.insert(planet, Name("Planet".into()));
    ed.world.insert(planet, Matter::Empty);
    ed.world.insert(
        planet,
        CelestialBody { mu: 1000.0, body_radius: 5.0, soi: 200.0, parent: "Star".into(), a: 1000.0, ..Default::default() },
    );
    let ball = ed.world.spawn();
    ed.world.insert(ball, Transform::IDENTITY);
    ed.world.insert(ball, Name("Ball".into()));
    ed.world.insert(ball, Matter::Empty);
    ed.world.insert(ball, RigidBody { gravity: false, radius: 0.5, ..Default::default() });
    // Put the planet and the ball where the rails have the planet at t = 0.
    ed.toggle_play();
    run(&mut ed, 1);
    let p = floptle_core::world_transform(&ed.world, planet).translation;
    ed.toggle_play();
    ed.world.get_mut::<Transform>(planet).unwrap().translation = p;
    ed.world.get_mut::<Transform>(ball).unwrap().translation = p + DVec3::new(0.0, 20.0, 0.0);
    ed.toggle_play();
    run(&mut ed, 100);
    let rel = lines(&mut ed, "REL");
    assert_eq!(rel.len(), 2, "{rel:?}");
    let num = |l: &str| -> Vec<f64> { l.split(' ').skip(1).map(|v| v.parse().unwrap()).collect() };
    let (a, b) = (num(&rel[0]), num(&rel[1]));
    let d = ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt();
    let moved = ((a[3] - b[3]).powi(2) + (a[4] - b[4]).powi(2) + (a[5] - b[5]).powi(2)).sqrt();
    assert!(moved > 50.0, "the planet should have moved on its rails meanwhile: {rel:?}");
    assert!(d < 0.5, "off for a second, it moved {d} m against its planet: {rel:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// An assembly root with one box part, built straight into the world.
fn vessel_node(ed: &mut crate::Editor, name: &str, at: DVec3, half: [f32; 3], mass: f32) -> floptle_core::Entity {
    let root = ed.world.spawn();
    ed.world.insert(root, Transform { translation: at, rotation: Quat::IDENTITY, scale: Vec3::ONE });
    ed.world.insert(root, Name(name.into()));
    ed.world.insert(root, Matter::Empty);
    ed.world.insert(root, RigidBody { assembly: true, ..Default::default() });
    let part = ed.world.spawn();
    ed.world.insert(part, Transform::IDENTITY);
    ed.world.insert(part, Name(format!("{name}Part")));
    ed.world.insert(part, Matter::Empty);
    ed.world.insert(part, floptle_core::Parent(root));
    ed.world.insert(part, RigidBody { kind: BodyKind::Box, half_extents: half, mass, ..Default::default() });
    root
}

/// From Lua: a servo hinge between two sub-assemblies is driven to a new
/// goal through its Joint handle each tick, with no rebuild, and reads its
/// angle back; a vessel grips the world where it hangs, holds there against
/// gravity, says so in `assembly.info`, and falls when it lets go.
#[test]
fn lua_drives_a_servo_between_sub_assemblies_and_grips_and_releases_a_vessel() {
    let (mut ed, dir) = project(
        "joints",
        r#"
local step = 0
function update(node, dt)
  step = step + 1
  local arm, lander = find("Arm"), find("Lander")
  local j = arm:getcomponent("Joint")
  if step == 2 then assembly.grip(lander, vec3(20, 5, 0)) end
  if step == 100 then
    log(string.format("SERVO a %.1f", j.position))
    j.goal = -10
  end
  if step == 200 then
    log(string.format("SERVO b %.1f", j.position))
    local i = assembly.info(lander)
    log(string.format("GRIP held %s %.2f", tostring(i.gripped), i.com.y))
    assembly.release(lander)
  end
  if step == 260 then
    local i = assembly.info(lander)
    log(string.format("GRIP let %s %.2f", tostring(i.gripped), i.com.y))
  end
end
"#,
    );
    let g = ed.world.spawn();
    ed.world.insert(g, Transform::IDENTITY);
    ed.world.insert(g, Matter::GravityVolume { mode: floptle_core::GravityMode::Down, strength: 9.81, radius: 1000.0 });
    let base = vessel_node(&mut ed, "Base", DVec3::new(0.0, 5.0, -10.0), [0.5, 0.5, 0.5], 50.0);
    ed.world.insert(base, floptle_core::joint::Joint { kind: floptle_core::joint::JointKind::Fixed, ..Default::default() });
    let arm = vessel_node(&mut ed, "Arm", DVec3::new(1.5, 5.0, -10.0), [1.0, 0.1, 0.1], 1.0);
    ed.world.insert(
        arm,
        floptle_core::joint::Joint {
            kind: floptle_core::joint::JointKind::Hinge,
            other: "Base".into(),
            anchor: [-1.0, 0.0, 0.0],
            motor: true,
            servo: true,
            goal: 30.0,
            motor_speed: 90.0,
            max_force: 500.0,
            ..Default::default()
        },
    );
    vessel_node(&mut ed, "Lander", DVec3::new(20.0, 5.0, 0.0), [0.5, 0.5, 0.5], 2.0);
    ed.toggle_play();
    run(&mut ed, 262);
    let servo = lines(&mut ed, "SERVO");
    assert_eq!(servo.len(), 2, "{servo:?}");
    let deg = |l: &str| -> f32 { l.split(' ').nth(1).unwrap().parse().unwrap() };
    assert!((deg(&servo[0]) - 30.0).abs() < 1.5, "servo should sit at 30°: {servo:?}");
    assert!((deg(&servo[1]) + 10.0).abs() < 1.5, "and drive to the new goal, -10°: {servo:?}");
    let grip = lines(&mut ed, "GRIP");
    assert_eq!(grip.len(), 2, "{grip:?}");
    let y = |l: &str| -> f32 { l.split(' ').nth(2).unwrap().parse().unwrap() };
    assert!(grip[0].starts_with("held true") && (y(&grip[0]) - 5.0).abs() < 0.05, "gripped, it hangs where it was: {grip:?}");
    assert!(grip[1].starts_with("let false") && y(&grip[1]) < 4.0, "released, it falls: {grip:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// What 500 small rails bodies cost a frame when nothing is near them.
/// A measurement, not a guard: `--ignored --nocapture`.
#[test]
#[ignore]
fn probe_belt_cost() {
    let measure = |n: usize| -> f64 {
        let (mut ed, dir) = project(&format!("belt{n}"), "function update(node, dt) end\n");
        for i in 0..n {
            let e = ed.world.spawn();
            ed.world.insert(e, Transform::IDENTITY);
            ed.world.insert(e, Name(format!("Rock{i}")));
            ed.world.insert(e, Matter::Empty);
            ed.world.insert(
                e,
                CelestialBody { mu: 0.0, body_radius: 30.0, soi: 1.0, parent: "Star".into(), a: 1.0e6 + i as f64 * 1000.0, m0: i as f64 * 0.01, ..Default::default() },
            );
        }
        let star = ed.world.query::<Name>().find(|(_, n)| n.0 == "Star").map(|(e, _)| e).unwrap();
        ed.world.get_mut::<CelestialBody>(star).unwrap().mu = 1.0e12;
        vessel_node(&mut ed, "Ship", DVec3::new(0.0, 0.0, -10.0), [0.5, 0.5, 0.5], 1.0);
        ed.toggle_play();
        run(&mut ed, 30);
        let t = floptle_core::time::Instant::now();
        run(&mut ed, 120);
        let ms = t.elapsed().as_secs_f64() * 1000.0 / 120.0;
        let _ = std::fs::remove_dir_all(&dir);
        ms
    };
    let base = measure(0);
    let belt = measure(500);
    floptle_say::say_err!("BELT frame {base:.3} ms without, {belt:.3} ms with 500 rails bodies (+{:.3})", belt - base);
}

/// The contract a game has today for a craft nobody is watching: kept live,
/// a drone 50 km from the camera simulates in full. It lands on a pad out
/// there, grips it, lets go and climbs away, with the camera at home.
#[test]
fn a_kept_live_drone_50_km_away_lands_grips_and_leaves_unwatched() {
    let (mut ed, dir) = project(
        "drone",
        r#"
local step, drone = 0, nil
function update(node, dt)
  step = step + 1
  drone = drone or find("Drone")
  if step == 1 then assembly.keepLive(drone, true) end
  local i = assembly.info(drone)
  if step == 150 then
    log(string.format("DRONE landed %s %.3f", tostring(i.grounded), i.com.y))
    assembly.grip(drone, vec3(i.com.x, i.com.y - 0.5, i.com.z))
  end
  if step == 200 then
    log(string.format("DRONE gripped %s %.3f", tostring(i.gripped), i.com.y))
    assembly.release(drone)
  end
  if step > 200 and step < 260 then assembly.force(drone, vec3(0, 3 * 9.81 * i.mass, 0)) end
  if step == 260 then log(string.format("DRONE left %s %.3f", tostring(i.gripped), i.com.y)) end
end
"#,
    );
    let far = DVec3::new(50_000.0, 0.0, 0.0);
    let g = ed.world.spawn();
    ed.world.insert(g, Transform::IDENTITY);
    ed.world.insert(g, Matter::GravityVolume { mode: floptle_core::GravityMode::Down, strength: 9.81, radius: 1.0e6 });
    let pad = ed.world.spawn();
    ed.world.insert(pad, Transform { translation: far + DVec3::new(0.0, -1.0, 0.0), rotation: Quat::IDENTITY, scale: Vec3::ONE });
    ed.world.insert(pad, Name("Pad".into()));
    ed.world.insert(pad, Matter::Empty);
    ed.world.insert(pad, RigidBody { kind: BodyKind::Box, mode: floptle_core::BodyMode::Static, half_extents: [5.0, 1.0, 5.0], ..Default::default() });
    let drone = vessel_node(&mut ed, "Drone", far + DVec3::new(0.0, 3.0, 0.0), [0.5, 0.5, 0.5], 2.0);
    ed.world.get_mut::<RigidBody>(drone).unwrap().gravity = true;
    ed.toggle_play();
    run(&mut ed, 262);
    let got = lines(&mut ed, "DRONE");
    assert_eq!(got.len(), 3, "{got:?}");
    let y = |l: &str| -> f32 { l.split(' ').nth(2).unwrap().parse().unwrap() };
    assert!(got[0].starts_with("landed true") && (y(&got[0]) - 0.5).abs() < 0.05, "it should be standing on the pad: {got:?}");
    assert!(got[1].starts_with("gripped true") && (y(&got[1]) - y(&got[0])).abs() < 0.02, "gripped, it holds: {got:?}");
    assert!(got[2].starts_with("left false") && y(&got[2]) > y(&got[0]) + 2.0, "released and pushed, it climbs away: {got:?}");
    assert!(!ed.compound_lod.contains_key(&drone.index()), "kept live, it never went to LOD");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A far craft nobody keeps live still answers a held force given from
/// `update`: it leaves the LOD and stays out while it is being pushed,
/// instead of being re-filed as frozen between one frame's thrust and the
/// next.
#[test]
fn a_far_craft_thrusting_from_update_climbs_out_of_lod() {
    let (mut ed, dir) = project(
        "thrust",
        r#"
local step = 0
function update(node, dt)
  step = step + 1
  local d = find("Drone")
  local i = assembly.info(d)
  if step > 30 and step <= 90 then assembly.force(d, vec3(0, 20 * i.mass, 0)) end
  if step == 30 or step == 91 then log(string.format("THRUST %d %.3f", step, i.com.y)) end
end
"#,
    );
    vessel_node(&mut ed, "Drone", DVec3::new(50_000.0, 3.0, 0.0), [0.5, 0.5, 0.5], 2.0);
    ed.toggle_play();
    run(&mut ed, 92);
    let got = lines(&mut ed, "THRUST");
    assert_eq!(got.len(), 2, "{got:?}");
    let y = |l: &str| -> f32 { l.split(' ').nth(1).unwrap().parse().unwrap() };
    assert!(y(&got[1]) > y(&got[0]) + 5.0, "a second of 20 m/s² should lift it well clear: {got:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
