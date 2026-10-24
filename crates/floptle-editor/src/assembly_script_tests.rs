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
/// anchored stand-in, and so does its root node's `vx`.
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
      log(string.format("INFO %.3f %s %s %d %.3f", inf.vel.x, tostring(inf.anchored), tostring(inf.grounded), frame, ship.vx))
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
        // Its root node reads the same velocity, not the stand-in's zero.
        let vx: f32 = it.nth(1).unwrap().parse().unwrap();
        assert!((vx - v).abs() <= 1e-3, "node.vx {vx}, assembly.info {v}: {info:?}");
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
/// planet while it is off, node and all, and is where it was relative to the
/// planet when it is switched back on.
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
  elseif step == 60 then
    log(string.format("REL off %.2f %.2f %.2f %.2f %.2f %.2f", rx, ry, rz, planet.x, planet.y, planet.z))
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
    assert_eq!(rel.len(), 3, "{rel:?}");
    let num = |l: &str| -> Vec<f64> { l.split(' ').skip(1).map(|v| v.parse().unwrap()).collect() };
    let (a, off, b) = (num(&rel[0]), num(&rel[1]), num(&rel[2]));
    // While it is off its node rides the planet too, not only its body.
    let d_off = ((a[0] - off[0]).powi(2) + (a[1] - off[1]).powi(2) + (a[2] - off[2]).powi(2)).sqrt();
    assert!(d_off < 0.5, "half a second off, its node is {d_off} m from where it was on its planet: {rel:?}");
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

/// A body a script moves while physics is paused is moved: its node's
/// transform carries the move the same frame, so every handle (a persistent
/// node's included) and every engine system that follows bodies sees it.
#[test]
fn a_body_moved_while_physics_is_paused_reaches_its_node() {
    let (mut ed, dir) = project(
        "pausedmove",
        r#"
function start(node) physics.pause(true) end
function update(node, dt)
  local b = find("Ball")
  b.x, b.y, b.z = 5000 + time, 0, 0
end
"#,
    );
    let ball = ed.world.spawn();
    ed.world.insert(ball, Transform::from_translation(DVec3::new(28.0, 190.0, -4739.0)));
    ed.world.insert(ball, Name("Ball".into()));
    ed.world.insert(ball, Matter::Empty);
    ed.world.insert(ball, RigidBody { gravity: false, radius: 0.5, ..Default::default() });
    ed.toggle_play();
    run(&mut ed, 30);
    let p = floptle_core::world_transform(&ed.world, ball).translation;
    assert!(p.x > 5000.0 && p.y.abs() < 1e-6 && p.z.abs() < 1e-6, "the ball's node is at {p:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A moving body stands still where physics paused it. Frames between ticks
/// used to draw it partway back along its last tick, so at 144 fps a craft
/// paused at 100 m/s shook by up to a tick's travel.
#[test]
fn a_body_paused_mid_flight_stands_still() {
    let (mut ed, dir) = project(
        "pausedstill",
        r#"
local n = 0
function update(node, dt)
  n = n + 1
  if n == 30 then physics.pause(true) end
end
"#,
    );
    let ball = ed.world.spawn();
    ed.world.insert(ball, Transform::IDENTITY);
    ed.world.insert(ball, Name("Ball".into()));
    ed.world.insert(ball, Matter::Empty);
    ed.world.insert(ball, RigidBody { gravity: false, radius: 0.5, ..Default::default() });
    ed.toggle_play();
    ed.sim.as_mut().unwrap().set_body_velocity(ball.index(), Vec3::new(100.0, 0.0, 0.0));
    let mut xs = Vec::new();
    for f in 0..60 {
        ed.play_step(1.0 / 144.0, true);
        if f >= 35 {
            xs.push(floptle_core::world_transform(&ed.world, ball).translation.x);
        }
    }
    let (lo, hi) = xs.iter().fold((f64::MAX, f64::MIN), |(lo, hi), &x| (lo.min(x), hi.max(x)));
    assert!(lo > 5.0, "the ball never flew: {xs:?}");
    assert!(hi - lo < 1e-6, "a paused ball moved {} m: {xs:?}", hi - lo);
    let _ = std::fs::remove_dir_all(&dir);
}

/// An impulse a rebuild callback aims at the centre of mass it has just
/// read lands there, on a planet moving on its rails too: the craft flies
/// off without a spin. It used to wait for the next drain, after a tick had
/// carried the craft a tick's travel of its planet away from the point, and
/// came out spinning at hundreds of radians a second.
#[test]
fn a_push_from_a_rebuild_callback_on_a_moving_planet_lands_at_the_centre_of_mass() {
    let (mut ed, dir) = project(
        "rebuildpush",
        r#"
local asked, ship, frame = false, nil, 0
function update(node, dt)
  frame = frame + 1
  local planet = find("Planet")
  if not planet then return end
  if not asked then
    asked = true
    local at = vec3(planet.x, planet.y + 30, planet.z)
    spawn("Vessel", at, function(v)
      ship = v
      spawn("Part", at, function(p)
        assembly.rebuild(v, function(s)
          local inf = assembly.info(s)
          assembly.impulseAt(s, vec3(0, 50 * inf.mass, 0), inf.com)
        end)
      end, v)
    end)
  elseif ship then
    local inf = assembly.info(ship)
    if inf then
      log(string.format("SPIN %.4f %.3f %d", math.sqrt(inf.angVel.x ^ 2 + inf.angVel.y ^ 2 + inf.angVel.z ^ 2), inf.vel.y, frame))
    end
  end
end
"#,
    );
    // A planet on a fast orbit about the star (100 m/s), with the camera on
    // it so the craft stays in full physics.
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
    ed.toggle_play();
    run(&mut ed, 1);
    let p = floptle_core::world_transform(&ed.world, planet).translation;
    ed.toggle_play();
    ed.world.get_mut::<Transform>(planet).unwrap().translation = p;
    let cam = ed.world.query::<Name>().find(|(_, n)| n.0 == "Camera").map(|(e, _)| e).unwrap();
    ed.world.get_mut::<Transform>(cam).unwrap().translation = p + DVec3::new(0.0, 0.0, 40.0);
    ed.toggle_play();
    run(&mut ed, 20);
    let spin = lines(&mut ed, "SPIN");
    assert!(spin.len() >= 10, "{spin:?}");
    for l in &spin {
        let mut it = l.split(' ');
        let w: f32 = it.next().unwrap().parse().unwrap();
        let vy: f32 = it.next().unwrap().parse().unwrap();
        assert!(w < 0.05, "spinning at {w} rad/s after a push at its centre of mass: {spin:?}");
        assert!((vy - 50.0).abs() < 1.0, "pushed to {vy} m/s, not 50: {spin:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A cold planet whose field is on disk streams in around a body a script
/// moves next to it while physics is paused, the way a loading screen parks
/// the crew over the world it is waiting for. Residency follows the body's
/// node, which only learnt about a paused move once the move reached it.
#[test]
fn a_cold_planet_streams_in_around_a_body_moved_to_it_while_paused() {
    let (mut ed, dir) = project(
        "pausedstream",
        r#"
function start(node) physics.pause(true) end
function update(node, dt)
  local rock, b = find("Rock"), find("Ball")
  if rock and b then b.x, b.y, b.z = rock.x, rock.y + 30, rock.z end
end
"#,
    );
    // A small planet on a wide orbit, its field written to the project the
    // way a saved world's is.
    let field = floptle_field::procgen::generate_planet(&floptle_field::procgen::PlanetFill {
        radius: 20.0,
        voxel: 1.5,
        relief: 2.0,
        cave_depth: 6.0,
        core_r: 3.0,
        seed: 1234,
        ..Default::default()
    });
    let path = ed.terrain_field_path_id(7);
    floptle_vfs::create_dir_all(path.parent().unwrap()).unwrap();
    floptle_vfs::write(&path, field.to_bytes()).unwrap();
    let rock = ed.world.spawn();
    ed.world.insert(rock, Transform::from_translation(DVec3::new(5000.0, -100_000.0, 0.0)));
    ed.world.insert(rock, Name("Rock".into()));
    ed.world.insert(rock, Matter::Terrain { id: 7, collision: Default::default() });
    ed.world.insert(rock, CelestialBody { mu: 0.0, body_radius: 20.0, parent: "Star".into(), a: 5000.0, ..Default::default() });
    let ball = ed.world.spawn();
    ed.world.insert(ball, Transform::IDENTITY);
    ed.world.insert(ball, Name("Ball".into()));
    ed.world.insert(ball, Matter::Empty);
    ed.world.insert(ball, RigidBody { gravity: false, radius: 0.5, ..Default::default() });
    ed.toggle_play();
    assert!(!ed.terrains.contains_key(&rock), "the planet should start cold for this test to mean anything");
    for _ in 0..30 {
        ed.play_step(1.0 / 60.0, true);
        ed.pump_world_streaming();
        if ed.terrains.contains_key(&rock) {
            break;
        }
    }
    let at = floptle_core::world_transform(&ed.world, ball).translation;
    let rp = floptle_core::world_transform(&ed.world, rock).translation;
    assert!(ed.terrains.contains_key(&rock), "the planet never streamed in; the ball's node is {} m from it", (at - rp).length());
    let _ = std::fs::remove_dir_all(&dir);
}

/// On a planet moving on its rails, a body spawned from `fixedUpdate` and
/// an assembly built from a spawn callback land where they were placed on
/// the planet, and stay there: both arrive before the next carry.
#[test]
fn spawns_and_rebuilds_on_a_moving_planet_land_where_they_were_placed() {
    let (mut ed, dir) = project(
        "railspawn",
        r#"
local n, ball, ship = 0, nil, nil
function fixedUpdate(node, dt)
  n = n + 1
  local planet = find("Planet")
  if not planet then return end
  local px, py, pz = planet.x, planet.y, planet.z
  if n == 5 then
    spawn("Part", vec3(px, py + 20, pz), function(p) ball = p end)
    spawn("Vessel", vec3(px + 10, py + 20, pz), function(v)
      ship = v
      spawn("Part", vec3(px + 10, py + 20, pz), function(p) assembly.rebuild(v) end, v)
    end)
  elseif n == 40 and ball and ship then
    local t = ball.tickPos
    local o = assembly.info(ship).origin
    log(string.format("REL %.4f %.4f %.4f %.4f %.4f %.4f", t.x - px, t.y - py, t.z - pz, o.x - px, o.y - py, o.z - pz))
  end
end
"#,
    );
    let star = ed.world.query::<Name>().find(|(_, n)| n.0 == "Star").map(|(e, _)| e).unwrap();
    ed.world.get_mut::<CelestialBody>(star).unwrap().mu = 1.0e8;
    let planet = ed.world.spawn();
    ed.world.insert(planet, Transform::IDENTITY);
    ed.world.insert(planet, Name("Planet".into()));
    ed.world.insert(planet, Matter::Empty);
    ed.world.insert(
        planet,
        CelestialBody { mu: 1000.0, body_radius: 5.0, soi: 200.0, parent: "Star".into(), a: 1000.0, ..Default::default() },
    );
    ed.toggle_play();
    run(&mut ed, 1);
    let p = floptle_core::world_transform(&ed.world, planet).translation;
    ed.toggle_play();
    ed.world.get_mut::<Transform>(planet).unwrap().translation = p;
    let cam = ed.world.query::<Name>().find(|(_, n)| n.0 == "Camera").map(|(e, _)| e).unwrap();
    ed.world.get_mut::<Transform>(cam).unwrap().translation = p + DVec3::new(0.0, 0.0, 40.0);
    ed.toggle_play();
    // Frames of 2.5 ticks, so spawns are asked for between ticks of a frame.
    for _ in 0..30 {
        ed.play_step(2.5 / 60.0, true);
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    }
    let rel = lines(&mut ed, "REL");
    assert_eq!(rel.len(), 1, "{rel:?}");
    let v: Vec<f64> = rel[0].split(' ').map(|x| x.parse().unwrap()).collect();
    let ball = ((v[0]).powi(2) + (v[1] - 20.0).powi(2) + v[2].powi(2)).sqrt();
    let ship = ((v[3] - 10.0).powi(2) + (v[4] - 20.0).powi(2) + v[5].powi(2)).sqrt();
    assert!(ball < 0.01, "the spawned body is {ball} m from where it was placed on its planet: {rel:?}");
    assert!(ship < 0.01, "the rebuilt craft is {ship} m from where it was placed on its planet: {rel:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A small body with no mass of its own, on a fast orbit, carries what is
/// standing just off it, as a planet does: its sphere of influence reaches
/// a few of its radii however small its Laplace sphere is.
#[test]
fn a_massless_rock_on_its_orbit_carries_what_is_beside_it() {
    let (mut ed, dir) = project(
        "rockride",
        r#"
local step = 0
function update(node, dt)
  step = step + 1
  local ball, rock = find("Ball"), find("Rock")
  if not (ball and rock) then return end
  if step == 10 or step == 70 then
    log(string.format("REL %.3f %.3f %.3f %.2f %.2f %.2f", ball.x - rock.x, ball.y - rock.y, ball.z - rock.z, rock.x, rock.y, rock.z))
  end
end
"#,
    );
    let star = ed.world.query::<Name>().find(|(_, n)| n.0 == "Star").map(|(e, _)| e).unwrap();
    ed.world.get_mut::<CelestialBody>(star).unwrap().mu = 1.0e7;
    let rock = ed.world.spawn();
    ed.world.insert(rock, Transform::IDENTITY);
    ed.world.insert(rock, Name("Rock".into()));
    ed.world.insert(rock, Matter::Empty);
    ed.world.insert(rock, CelestialBody { mu: 0.0, body_radius: 10.0, parent: "Star".into(), a: 1000.0, ..Default::default() });
    let ball = ed.world.spawn();
    ed.world.insert(ball, Transform::IDENTITY);
    ed.world.insert(ball, Name("Ball".into()));
    ed.world.insert(ball, Matter::Empty);
    ed.world.insert(ball, RigidBody { gravity: false, radius: 0.5, ..Default::default() });
    ed.toggle_play();
    run(&mut ed, 1);
    let p = floptle_core::world_transform(&ed.world, rock).translation;
    ed.toggle_play();
    ed.world.get_mut::<Transform>(rock).unwrap().translation = p;
    ed.world.get_mut::<Transform>(ball).unwrap().translation = p + DVec3::new(0.0, 15.0, 0.0);
    let cam = ed.world.query::<Name>().find(|(_, n)| n.0 == "Camera").map(|(e, _)| e).unwrap();
    ed.world.get_mut::<Transform>(cam).unwrap().translation = p + DVec3::new(0.0, 0.0, 40.0);
    ed.toggle_play();
    run(&mut ed, 80);
    let rel = lines(&mut ed, "REL");
    assert_eq!(rel.len(), 2, "{rel:?}");
    let num = |l: &str| -> Vec<f64> { l.split(' ').map(|v| v.parse().unwrap()).collect() };
    let (a, b) = (num(&rel[0]), num(&rel[1]));
    let moved = ((a[3] - b[3]).powi(2) + (a[4] - b[4]).powi(2) + (a[5] - b[5]).powi(2)).sqrt();
    let d = ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt();
    assert!(moved > 50.0, "the rock should have moved on its orbit: {rel:?}");
    assert!(d < 0.5, "the ball beside the rock was left {d} m behind it: {rel:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The rock's reach is its own: at 2 radii the ball 15 m off a 10 m rock
/// rides it, at 1 radius (its ground) it is left behind.
fn ball_left_behind_by_a_rock(reach: f64) -> f64 {
    let (mut ed, dir) = project(
        &format!("rockreach{reach}"),
        r#"
local step = 0
function update(node, dt)
  step = step + 1
  local ball, rock = find("Ball"), find("Rock")
  if not (ball and rock) then return end
  if step == 10 or step == 70 then
    log(string.format("REL %.3f %.3f %.3f %.2f %.2f %.2f", ball.x - rock.x, ball.y - rock.y, ball.z - rock.z, rock.x, rock.y, rock.z))
  end
end
"#,
    );
    let star = ed.world.query::<Name>().find(|(_, n)| n.0 == "Star").map(|(e, _)| e).unwrap();
    ed.world.get_mut::<CelestialBody>(star).unwrap().mu = 1.0e7;
    let rock = ed.world.spawn();
    ed.world.insert(rock, Transform::IDENTITY);
    ed.world.insert(rock, Name("Rock".into()));
    ed.world.insert(rock, Matter::Empty);
    ed.world.insert(rock, CelestialBody { mu: 0.0, body_radius: 10.0, parent: "Star".into(), a: 1000.0, reach, ..Default::default() });
    let ball = ed.world.spawn();
    ed.world.insert(ball, Transform::IDENTITY);
    ed.world.insert(ball, Name("Ball".into()));
    ed.world.insert(ball, Matter::Empty);
    ed.world.insert(ball, RigidBody { gravity: false, radius: 0.5, ..Default::default() });
    ed.toggle_play();
    run(&mut ed, 1);
    let p = floptle_core::world_transform(&ed.world, rock).translation;
    ed.toggle_play();
    ed.world.get_mut::<Transform>(rock).unwrap().translation = p;
    ed.world.get_mut::<Transform>(ball).unwrap().translation = p + DVec3::new(0.0, 15.0, 0.0);
    let cam = ed.world.query::<Name>().find(|(_, n)| n.0 == "Camera").map(|(e, _)| e).unwrap();
    ed.world.get_mut::<Transform>(cam).unwrap().translation = p + DVec3::new(0.0, 0.0, 40.0);
    ed.toggle_play();
    run(&mut ed, 80);
    let rel = lines(&mut ed, "REL");
    assert_eq!(rel.len(), 2, "{rel:?}");
    let num = |l: &str| -> Vec<f64> { l.split(' ').map(|v| v.parse().unwrap()).collect() };
    let (a, b) = (num(&rel[0]), num(&rel[1]));
    let moved = ((a[3] - b[3]).powi(2) + (a[4] - b[4]).powi(2) + (a[5] - b[5]).powi(2)).sqrt();
    let d = ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt();
    assert!(moved > 50.0, "the rock should have moved on its orbit: {rel:?}");
    let _ = std::fs::remove_dir_all(&dir);
    d
}

#[test]
fn a_rocks_reach_is_set_per_body() {
    assert!(ball_left_behind_by_a_rock(2.0) < 0.5, "a reach of 2 radii should carry a ball 1.5 radii out");
    assert!(ball_left_behind_by_a_rock(1.0) > 50.0, "a reach of 1 radius should leave a ball 1.5 radii out behind");
}

/// A craft a script teleports is where it was sent from the next tick on,
/// as its node reads it: within one frame of several ticks, a tick after
/// the teleport reads the new place, not the old one. A landing claw that
/// cast its ray from the node's old place gripped ground 190 m behind the
/// craft, and the weld tore it apart.
#[test]
fn a_teleported_craft_reads_where_it_was_sent_on_the_next_tick() {
    let (mut ed, dir) = project(
        "teleportread",
        r#"
local n, ship = 0, nil
function fixedUpdate(node, dt)
  n = n + 1
  if n == 1 then
    spawn("Vessel", vec3(0, 0, 0), function(v)
      spawn("Part", vec3(0, 0, 0), function(p) assembly.rebuild(v, function(s) ship = s end) end, v)
    end)
  elseif ship and n == 10 then
    assembly.teleport(ship, vec3(200, 0, 0))
  elseif ship and n == 11 then
    log(string.format("AT %.2f", ship.x))
  end
end
"#,
    );
    ed.toggle_play();
    // Frames of three ticks, so the tick after the teleport is in its frame.
    for _ in 0..8 {
        ed.play_step(3.0 / 60.0, true);
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    }
    let at = lines(&mut ed, "AT");
    assert_eq!(at.len(), 1, "{at:?}");
    let x: f64 = at[0].parse().unwrap();
    assert!((x - 200.0).abs() < 0.5, "the tick after the teleport the craft's node read x = {x}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A lake declared on a planet moving fast on its rails, on the face the
/// planet is moving along, floats a light body at its surface, where it is
/// drawn: the water rides the planet every tick, not once a frame behind it.
#[test]
fn a_declared_lake_on_a_fast_planet_floats_a_body_at_its_surface() {
    let (mut ed, dir) = project(
        "lakeride",
        r#"
local n, made = 0, false
function update(node, dt)
  n = n + 1
  local ball, planet = find("Ball"), find("Planet")
  if not (ball and planet) then return end
  if not made then
    made = true
    water.addVolume{ shape = "lake", parent = "Planet", center = vec3(0, 0, 100), reach = 40 }
  end
  if n % 20 == 0 then
    local dx, dy, dz = ball.x - planet.x, ball.y - planet.y, ball.z - planet.z
    log(string.format("H %d %.3f", n, math.sqrt(dx * dx + dy * dy + dz * dz)))
  end
end
"#,
    );
    let star = ed.world.query::<Name>().find(|(_, n)| n.0 == "Star").map(|(e, _)| e).unwrap();
    ed.world.get_mut::<CelestialBody>(star).unwrap().mu = 7.84e7;
    let planet = ed.world.spawn();
    ed.world.insert(planet, Transform::IDENTITY);
    ed.world.insert(planet, Name("Planet".into()));
    ed.world.insert(planet, Matter::Empty);
    ed.world.insert(planet, CelestialBody { mu: 9.8e4, body_radius: 95.0, soi: 2000.0, parent: "Star".into(), a: 1000.0, ..Default::default() });
    let ball = ed.world.spawn();
    ed.world.insert(ball, Transform::IDENTITY);
    ed.world.insert(ball, Name("Ball".into()));
    ed.world.insert(ball, Matter::Empty);
    ed.world.insert(ball, RigidBody { radius: 0.5, mass: 20.0, ..Default::default() });
    ed.toggle_play();
    run(&mut ed, 1);
    let p = floptle_core::world_transform(&ed.world, planet).translation;
    ed.toggle_play();
    ed.world.get_mut::<Transform>(planet).unwrap().translation = p;
    ed.world.get_mut::<Transform>(ball).unwrap().translation = p + DVec3::new(0.0, 0.0, 100.6);
    let cam = ed.world.query::<Name>().find(|(_, n)| n.0 == "Camera").map(|(e, _)| e).unwrap();
    ed.world.get_mut::<Transform>(cam).unwrap().translation = p + DVec3::new(20.0, 0.0, 110.0);
    ed.toggle_play();
    // Frames of three ticks: the water is built once a frame before them.
    for _ in 0..120 {
        ed.play_step(3.0 / 60.0, true);
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    }
    let h = lines(&mut ed, "H");
    let last: Vec<f64> = h.iter().rev().take(4).map(|l| l.split(' ').nth(1).unwrap().parse().unwrap()).collect();
    for r in &last {
        assert!((r - 100.0).abs() < 0.6, "the ball floats at {r} from the planet's centre, the lake's surface is at 100: {h:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A ray, a sphere cast and an overlap through each part of a four-part
/// craft each report that part, in a flat scene and on a planet moving on
/// its rails; and a ray told to ignore the craft passes through all of it.
fn queries_find_each_part(on_rails: bool) {
    let (mut ed, dir) = project(
        if on_rails { "partqueryrails" } else { "partquery" },
        r#"
local n, ship, parts, base = 0, nil, {}, nil
function fixedUpdate(node, dt)
  n = n + 1
  local planet = find("Planet")
  base = planet and vec3(planet.x, planet.y, planet.z) or vec3(0, 0, 0)
  if n == 2 then
    spawn("Vessel", base, function(v)
      ship = v
      for i = 1, 4 do
        spawn("Part", vec3(base.x + (i - 1) * 2, base.y, base.z), function(p)
          parts[i] = p
          if #parts == 4 then assembly.rebuild(v) end
        end, v)
      end
    end)
  elseif n == 10 and ship then
    for i = 1, 4 do
      local x = base.x + (i - 1) * 2
      local r = raycast(x, base.y + 5, base.z, 0, -1, 0, 10)
      local s = spherecast(vec3(x, base.y + 5, base.z), vec3(0, -1, 0), 0.2, 10)
      local o = overlapSphere(vec3(x, base.y, base.z), 0.3)
      local on = o[1] and o[1].node
      local id = parts[i].__id
      log(string.format("PART %d %s %s %s", i, tostring(r and r.node.__id == id), tostring(s and s.node.__id == id), tostring(on and on.__id == id)))
    end
    local through = raycast(base.x, base.y + 5, base.z, 0, -1, 0, 10, ship)
    log("IGNORED " .. tostring(through == nil or through.node.__id ~= parts[1].__id))
  end
end
"#,
    );
    if on_rails {
        let star = ed.world.query::<Name>().find(|(_, n)| n.0 == "Star").map(|(e, _)| e).unwrap();
        ed.world.get_mut::<CelestialBody>(star).unwrap().mu = 1.0e7;
        let planet = ed.world.spawn();
        ed.world.insert(planet, Transform::IDENTITY);
        ed.world.insert(planet, Name("Planet".into()));
        ed.world.insert(planet, Matter::Empty);
        ed.world.insert(planet, CelestialBody { mu: 0.0, body_radius: 5.0, soi: 200.0, parent: "Star".into(), a: 1000.0, ..Default::default() });
        ed.toggle_play();
        run(&mut ed, 1);
        let p = floptle_core::world_transform(&ed.world, planet).translation;
        ed.toggle_play();
        ed.world.get_mut::<Transform>(planet).unwrap().translation = p;
        let cam = ed.world.query::<Name>().find(|(_, n)| n.0 == "Camera").map(|(e, _)| e).unwrap();
        ed.world.get_mut::<Transform>(cam).unwrap().translation = p + DVec3::new(0.0, 0.0, 30.0);
    }
    ed.toggle_play();
    run(&mut ed, 14);
    let parts = lines(&mut ed, "PART");
    assert_eq!(parts.len(), 4, "{parts:?}");
    for l in &parts {
        assert!(l.ends_with("true true true"), "a query missed its part (rails: {on_rails}): {parts:?}");
    }
    assert_eq!(lines(&mut ed, "IGNORED"), vec!["true".to_string()], "a ray told to ignore the craft hit it");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rays_casts_and_overlaps_find_an_assemblys_parts() {
    queries_find_each_part(false);
    queries_find_each_part(true);
}

/// In each `fixedUpdate` of a frame of several ticks, `assembly.info` gives
/// a spinning craft's axes as they are that tick: from one tick to the next
/// they turn by the spin over a tick, not stand still until the frame ends.
#[test]
fn assembly_info_turns_with_its_craft_tick_by_tick() {
    let (mut ed, dir) = project(
        "infopose",
        r#"
local n, ship, last = 0, nil, nil
function fixedUpdate(node, dt)
  n = n + 1
  if n == 1 then
    spawn("Vessel", vec3(0, 0, 0), function(v)
      spawn("Part", vec3(1, 0, 0), function(p) assembly.rebuild(v, function(s) ship = s end) end, v)
    end)
  elseif ship then
    local r = assembly.info(ship).right
    if last and n > 6 then
      local dot = math.max(-1, math.min(1, r.x * last.x + r.y * last.y + r.z * last.z))
      log(string.format("TURN %.5f %d", math.acos(dot), n))
    end
    last = r
  end
end
"#,
    );
    ed.toggle_play();
    // Frames of three ticks.
    for _ in 0..3 {
        ed.play_step(3.0 / 60.0, true);
    }
    let root = ed.world.query::<Name>().find(|(_, n)| n.0 == "Vessel").map(|(e, _)| e.index()).unwrap();
    ed.sim.as_mut().unwrap().compound_of_mut(root).unwrap().ang_vel = Vec3::new(0.0, 2.0, 0.0);
    for _ in 0..6 {
        ed.play_step(3.0 / 60.0, true);
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    }
    let turns = lines(&mut ed, "TURN");
    assert!(turns.len() >= 9, "{turns:?}");
    // 2 rad/s over a sixtieth of a second, every tick.
    for t in turns.iter().skip(3) {
        let a: f64 = t.split(' ').next().unwrap().parse().unwrap();
        assert!((a - 2.0 / 60.0).abs() < 0.004, "a tick turned it {a} rad, not {}: {turns:?}", 2.0 / 60.0);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A four-wheeled rover standing on its wheels on a planet moving on its
/// rails, creeping along at half a metre a second, can time-warp without
/// being anchored: on its wheels it is on the ground, not in flight, and was
/// put on an orbit that dropped the warp for being near the surface.
#[test]
fn a_rover_parked_on_its_wheels_can_time_warp() {
    let (mut ed, dir) = project(
        "roverwarp",
        r#"
local n = 0
function update(node, dt)
  n = n + 1
  -- Asked for once, then left alone: whatever drops it, drops it for good.
  if n == 120 then space.warp(10) end
  local r, p = find("Rover"), find("Planet")
  if n > 180 and r and p then
    local i = assembly.info(r)
    local dx, dy, dz = i.com.x - p.x, i.com.y - p.y, i.com.z - p.z
    log(string.format("WARP %.1f %.3f %s %d", space.warp(), math.sqrt(dx * dx + dy * dy + dz * dz), tostring(i.grounded), n))
  end
end
"#,
    );
    let star = ed.world.query::<Name>().find(|(_, n)| n.0 == "Star").map(|(e, _)| e).unwrap();
    ed.world.get_mut::<CelestialBody>(star).unwrap().mu = 1.0e7;
    let planet = ed.world.spawn();
    ed.world.insert(planet, Transform::IDENTITY);
    ed.world.insert(planet, Name("Planet".into()));
    ed.world.insert(planet, Matter::Empty);
    ed.world.insert(planet, CelestialBody { mu: 6.5 * 2500.0, body_radius: 50.0, soi: 500.0, parent: "Star".into(), a: 1000.0, ..Default::default() });
    ed.toggle_play();
    run(&mut ed, 1);
    let p = floptle_core::world_transform(&ed.world, planet).translation;
    ed.toggle_play();
    ed.world.get_mut::<Transform>(planet).unwrap().translation = p;
    // Ground on the planet's top: a static slab parented to it.
    let pad = ed.world.spawn();
    ed.world.insert(pad, Transform::from_translation(DVec3::new(0.0, 49.0, 0.0)));
    ed.world.insert(pad, Name("Pad".into()));
    ed.world.insert(pad, Matter::Empty);
    ed.world.insert(pad, floptle_core::Parent(planet));
    ed.world.insert(pad, RigidBody { kind: BodyKind::Box, mode: floptle_core::BodyMode::Static, half_extents: [10.0, 1.0, 10.0], friction: 1.0, ..Default::default() });
    let at = p + DVec3::new(0.0, 50.9, 0.0);
    let chassis = vessel_node(&mut ed, "Rover", at, [0.8, 0.15, 0.5], 0.6);
    for (dx, dz) in [(-0.7, -0.65), (0.7, -0.65), (-0.7, 0.65), (0.7, 0.65)] {
        let w = ed.world.spawn();
        ed.world.insert(w, Transform::from_translation(at + DVec3::new(dx, -0.35, dz)));
        ed.world.insert(w, Name(format!("Wheel{dx}{dz}")));
        ed.world.insert(w, Matter::Empty);
        ed.world.insert(w, RigidBody { radius: 0.35, mass: 0.1, friction: 1.0, rotates: true, ..Default::default() });
        ed.world.insert(
            w,
            floptle_core::joint::Joint {
                kind: floptle_core::joint::JointKind::Wheel,
                other: "Rover".into(),
                axis: [0.0, 1.0, 0.0],
                stiffness: 5.0,
                damping: 0.8,
                // Rolling slowly, as a parked rover creeps on its springs.
                motor: true,
                motor_speed: 90.0,
                max_force: 5.0,
                ..Default::default()
            },
        );
    }
    let _ = chassis;
    let cam = ed.world.query::<Name>().find(|(_, n)| n.0 == "Camera").map(|(e, _)| e).unwrap();
    ed.world.get_mut::<Transform>(cam).unwrap().translation = at + DVec3::new(0.0, 2.0, 15.0);
    ed.toggle_play();
    run(&mut ed, 260);
    let warp = lines(&mut ed, "WARP");
    assert!(!warp.is_empty(), "{warp:?}");
    // From a second after it was asked for, every frame: still warping,
    // still on the ground (the pad's top is 50 m from the centre), and
    // reading as grounded.
    for l in &warp {
        let v: Vec<&str> = l.split(' ').collect();
        let (w, r): (f64, f64) = (v[0].parse().unwrap(), v[1].parse().unwrap());
        assert!(w >= 10.0 - 1e-6, "on its wheels, the rover dropped the warp to {w}: {warp:?}");
        assert!(r > 50.0 && r < 51.5, "the rover left the ground for {r} from the centre: {warp:?}");
        assert_eq!(v[2], "true", "on its wheels it reads as not grounded: {warp:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A Kinematic or Static post spawned on a planet moving on its rails at
/// 100 m/s stops a crew capsule (r 0.3, 2 m tall) walking into it at its
/// surface: the post's radius and the crew's from its axis. Parented to the
/// planet and moved by script through `node.x`, or at the root with its
/// place written each tick as the planet's plus an offset. A sphere post at
/// the crew's middle went between the capsule's end spheres when it was
/// kinematic: those were all a capsule tested against a kinematic body.
/// Returns the closest the crew's axis came to the post's centre, and the
/// Console lines saying a kinematic body was moved outside `fixedUpdate`.
fn post_stops_a_walker(mode: floptle_core::BodyMode, kind: BodyKind, parented: bool) -> f64 {
    post_walk(mode, kind, parented, "fixedUpdate").0
}

fn post_walk(mode: floptle_core::BodyMode, kind: BodyKind, parented: bool, pass: &str) -> (f64, Vec<String>) {
    // Tests run in parallel and two can ask for the same post: one project each.
    static RUN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let run_no = RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tag = format!("postwalk{mode:?}{kind:?}{parented}{pass}{run_no}");
    let place = if parented {
        // Moved by script each tick, in the planet's frame.
        "post.x = 4 + math.min(n, 60) / 60"
    } else {
        "post.pos = p.pos + vec3(4 + math.min(n, 60) / 60, 0, 0)"
    };
    let script = format!(
        r#"
local n, post = 0, nil
function {pass}(node, dt)
  n = n + 1
  local p, ball = find("Planet"), find("Ball")
  if not (p and ball) then return end
  if n == 2 then
    spawn("Post", vec3(p.x + 8, p.y, p.z), function(q) post = q end, {parent})
  end
  if post then {place} end
  -- Steered into it, as a walker is.
  if n >= 70 then ball.vx = 1.0 end
  if post and n > 70 then
    -- The ball's own place this tick, not its render pose, which trails the
    -- planet's by up to a tick of its 100 m/s.
    local w, b = post:toWorld(vec3(0, 0, 0)), ball.tickPos
    log(string.format("D %.4f %d", math.sqrt((b.x - w.x) ^ 2 + (b.z - w.z) ^ 2), n))
  end
end
"#,
        parent = if parented { "p" } else { "nil" },
    );
    let (mut ed, dir) = project(&tag, &script);
    prefab(&dir, "Post", RigidBody { kind, mode, radius: 0.5, height: 3.0, gravity: false, ..Default::default() });
    let star = ed.world.query::<Name>().find(|(_, n)| n.0 == "Star").map(|(e, _)| e).unwrap();
    ed.world.get_mut::<CelestialBody>(star).unwrap().mu = 1.0e7;
    let planet = ed.world.spawn();
    ed.world.insert(planet, Transform::IDENTITY);
    ed.world.insert(planet, Name("Planet".into()));
    ed.world.insert(planet, Matter::Empty);
    ed.world.insert(planet, CelestialBody { mu: 0.0, body_radius: 5.0, soi: 500.0, parent: "Star".into(), a: 1000.0, ..Default::default() });
    let ball = ed.world.spawn();
    ed.world.insert(ball, Transform::IDENTITY);
    ed.world.insert(ball, Name("Ball".into()));
    ed.world.insert(ball, Matter::Empty);
    ed.world.insert(ball, RigidBody { kind: BodyKind::Capsule, gravity: false, radius: 0.3, height: 2.0, ..Default::default() });
    ed.toggle_play();
    run(&mut ed, 1);
    let p = floptle_core::world_transform(&ed.world, planet).translation;
    ed.toggle_play();
    ed.world.get_mut::<Transform>(planet).unwrap().translation = p;
    ed.world.get_mut::<Transform>(ball).unwrap().translation = p;
    let cam = ed.world.query::<Name>().find(|(_, n)| n.0 == "Camera").map(|(e, _)| e).unwrap();
    ed.world.get_mut::<Transform>(cam).unwrap().translation = p + DVec3::new(0.0, 5.0, 20.0);
    ed.toggle_play();
    run(&mut ed, 500);
    let d = lines(&mut ed, "D");
    assert!(d.len() > 100, "{d:?}");
    let _ = std::fs::remove_dir_all(&dir);
    let warned = ed
        .console
        .entries
        .iter()
        .filter(|e| e.msg.contains("was moved outside fixedUpdate"))
        .map(|e| format!("{} x{}", e.msg, e.count))
        .collect();
    (d.iter().map(|l| l.split(' ').next().unwrap().parse::<f64>().unwrap()).fold(f64::MAX, f64::min), warned)
}

#[test]
fn a_script_moved_post_on_a_moving_planet_stops_a_walker() {
    use floptle_core::BodyMode::{Kinematic, Static};
    let mut missed = Vec::new();
    for (mode, kind, parented) in [
        (Kinematic, BodyKind::Capsule, true),
        (Kinematic, BodyKind::Sphere, true),
        (Kinematic, BodyKind::Sphere, false),
        (Static, BodyKind::Capsule, true),
        (Static, BodyKind::Sphere, true),
    ] {
        let closest = post_stops_a_walker(mode, kind, parented);
        // Radii 0.5 + 0.3.
        if closest < 0.75 {
            missed.push(format!("{mode:?} {kind:?} parented {parented}: {closest:.3} m"));
        }
    }
    assert!(missed.is_empty(), "the walker came closer to the post's axis than its radius and the post's: {missed:?}");
}

/// The same root post placed from `update` stands one carry behind where it
/// was put for the whole tick, because its world moves at the start of the
/// tick and a kinematic body follows its node. Where the script meant it is
/// the game's call, so the engine leaves it there and says so once, naming
/// the post and the carry (100 m/s over a 60 Hz tick). Placed from
/// `fixedUpdate`, or riding its planet as a child, nothing is said.
#[test]
fn a_kinematic_placed_from_update_on_a_moving_planet_is_reported_once() {
    use floptle_core::BodyMode::Kinematic;
    let (_, warned) = post_walk(Kinematic, BodyKind::Sphere, false, "update");
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert!(warned[0].contains("\"Post\"") && warned[0].contains("1.67 m") && warned[0].ends_with(" x1"), "{warned:?}");
    for parented in [false, true] {
        let (_, warned) = post_walk(Kinematic, BodyKind::Sphere, parented, "fixedUpdate");
        assert!(warned.is_empty(), "parented {parented}, placed from fixedUpdate: {warned:?}");
    }
    let (_, warned) = post_walk(Kinematic, BodyKind::Sphere, true, "update");
    assert!(warned.is_empty(), "a child of the planet rides it: {warned:?}");
}

/// A thousand Kinematic-box parts laid as a floor by `spawn`, and a crate
/// thrown down onto them. With `{ body = false }` the parts arrive as nodes and no
/// more: the crate is the scene's only body, and it falls through. Returns
/// the crate's height, how many bodies the scene has and how many parts
/// arrived.
fn display_floor(bodies: bool) -> (f64, usize, usize) {
    let script = format!(
        r#"
local n, crate = 0, nil
function fixedUpdate(node, dt)
  n = n + 1
  if n == 2 then
    for i = 0, 999 do
      spawn("Beam", vec3(i % 32 - 16, 0, math.floor(i / 32) - 16), nil, {{ body = {bodies} }})
    end
  end
  -- Thrown down: the scene's only gravity is its near-massless star's.
  if n == 5 then spawn("Crate", vec3(0.2, 3, 0.2), function(c) crate = c; c.vy = -3 end) end
  if crate and n % 10 == 0 then log(string.format("Y %.4f", crate.tickPos.y)) end
end
"#
    );
    let (mut ed, dir) = project(&format!("displayfloor{bodies}"), &script);
    prefab(
        &dir,
        "Beam",
        RigidBody { kind: BodyKind::Box, mode: floptle_core::BodyMode::Kinematic, half_extents: [0.5, 0.5, 0.5], ..Default::default() },
    );
    prefab(&dir, "Crate", RigidBody { kind: BodyKind::Box, half_extents: [0.4, 0.4, 0.4], ..Default::default() });
    ed.toggle_play();
    run(&mut ed, 120);
    let y: f64 = lines(&mut ed, "Y").last().expect("the crate never logged").parse().unwrap();
    let count = ed.sim.as_ref().unwrap().body_states().count();
    let parts = ed.world.query::<Name>().filter(|(_, n)| n.0 == "Beam").count();
    let _ = std::fs::remove_dir_all(&dir);
    (y, count, parts)
}

/// `spawn(prefab, pos, fn, { body = false })` spawns a display copy: the
/// prefab's nodes without its body, so a thousand parts cost physics
/// nothing. Spawned as usual, the same parts are a floor the crate lands on.
#[test]
fn a_prefab_spawned_without_its_body_costs_physics_nothing() {
    let (y, count, parts) = display_floor(false);
    assert_eq!(parts, 1000, "the parts did not all arrive");
    assert_eq!(count, 1, "{count} bodies: the display copies brought theirs");
    assert!(y < -1.0, "the crate rests at {y} on parts with no body");
    let (y, count, parts) = display_floor(true);
    assert_eq!((parts, count), (1000, 1001));
    // The floor's top is at 0.5; the crate is 0.4 half-high.
    assert!((y - 0.9).abs() < 0.05, "the crate came to rest at {y}, not on the parts");
}

/// A body under a parent node that a script moves with `node.x` goes where
/// its node says, in the parent's frame, not to that number in the world.
#[test]
fn a_parented_body_moved_by_script_goes_where_its_node_says() {
    let (mut ed, dir) = project(
        "parentedmove",
        r#"
local n = 0
function update(node, dt)
  n = n + 1
  local b = find("Ball")
  if b and n == 3 then b.x = 5 end
end
"#,
    );
    let holder = ed.world.spawn();
    ed.world.insert(holder, Transform::from_translation(DVec3::new(100.0, 20.0, 0.0)));
    ed.world.insert(holder, Name("Holder".into()));
    ed.world.insert(holder, Matter::Empty);
    let ball = ed.world.spawn();
    ed.world.insert(ball, Transform::IDENTITY);
    ed.world.insert(ball, Name("Ball".into()));
    ed.world.insert(ball, Matter::Empty);
    ed.world.insert(ball, floptle_core::Parent(holder));
    ed.world.insert(ball, RigidBody { gravity: false, radius: 0.3, ..Default::default() });
    ed.toggle_play();
    run(&mut ed, 8);
    let body = ed.sim.as_ref().unwrap().body_states().find(|r| r.entity == ball).unwrap().pos;
    let node = floptle_core::world_transform(&ed.world, ball).translation;
    assert!((body - DVec3::new(105.0, 20.0, 0.0)).length() < 1e-3, "the body went to {body:?}");
    assert!((node - DVec3::new(105.0, 20.0, 0.0)).length() < 1e-3, "the node is at {node:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Debris thrown upward far from the camera, where craft go to their
/// distant LOD, comes down and lands: slow at the top of its arc is not
/// resting, and it is not frozen there in mid-air.
#[test]
fn far_debris_thrown_up_lands() {
    let (mut ed, dir) = project(
        "fardebris",
        r#"
local n, ship = 0, nil
function fixedUpdate(node, dt)
  n = n + 1
  local p = find("Planet")
  if not p then return end
  if n == 30 then
    spawn("Debris", vec3(p.x, p.y + 52, p.z), function(v)
      spawn("Part", vec3(p.x, p.y + 52, p.z), function(q)
        assembly.rebuild(v, function(s) ship = s; assembly.impulseAt(s, vec3(0, 8 * assembly.info(s).mass, 0), assembly.info(s).com) end)
      end, v)
    end)
  end
  if ship and n % 10 == 0 then
    local c = assembly.info(ship).com
    log(string.format("H %.3f %d", math.sqrt((c.x - p.x) ^ 2 + (c.y - p.y) ^ 2 + (c.z - p.z) ^ 2), n))
  end
end
"#,
    );
    // Falls, unlike the helper's floating Vessel.
    prefab(&dir, "Debris", RigidBody { assembly: true, ..Default::default() });
    let star = ed.world.query::<Name>().find(|(_, n)| n.0 == "Star").map(|(e, _)| e).unwrap();
    ed.world.get_mut::<CelestialBody>(star).unwrap().mu = 1.0e7;
    let planet = ed.world.spawn();
    ed.world.insert(planet, Transform::IDENTITY);
    ed.world.insert(planet, Name("Planet".into()));
    ed.world.insert(planet, Matter::Empty);
    ed.world.insert(planet, CelestialBody { mu: 9.8 * 2500.0, body_radius: 50.0, soi: 2000.0, parent: "Star".into(), a: 5000.0, ..Default::default() });
    ed.toggle_play();
    run(&mut ed, 1);
    let p = floptle_core::world_transform(&ed.world, planet).translation;
    ed.toggle_play();
    ed.world.get_mut::<Transform>(planet).unwrap().translation = p;
    let pad = ed.world.spawn();
    ed.world.insert(pad, Transform::from_translation(DVec3::new(0.0, 49.0, 0.0)));
    ed.world.insert(pad, Name("Pad".into()));
    ed.world.insert(pad, Matter::Empty);
    ed.world.insert(pad, floptle_core::Parent(planet));
    ed.world.insert(pad, RigidBody { kind: BodyKind::Box, mode: floptle_core::BodyMode::Static, half_extents: [10.0, 1.0, 10.0], ..Default::default() });
    // The camera 900 m off: the debris is in its distant LOD.
    let cam = ed.world.query::<Name>().find(|(_, n)| n.0 == "Camera").map(|(e, _)| e).unwrap();
    ed.world.get_mut::<Transform>(cam).unwrap().translation = p + DVec3::new(900.0, 0.0, 0.0);
    ed.toggle_play();
    run(&mut ed, 400);
    let h = lines(&mut ed, "H");
    let last: f64 = h.last().expect("no heights").split(' ').next().unwrap().parse().unwrap();
    // The pad's top is 50 from the centre and the part a unit box.
    assert!((last - 50.5).abs() < 0.3, "the debris is {last} from the planet's centre, not landed on the pad: {h:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A kinematic tyre a script keeps on its mount on a planet moving at
/// 280 m/s, across floating-origin recentres. With `node.carry` added to the
/// place it keeps, the tyre stays on the mount; without it, it falls behind
/// by exactly the carry.
fn tyre_offset_after(use_carry: bool) -> (f64, f64) {
    let script = format!(
        r#"
local n, pos, carried, off = 0, nil, vec3(0, 0, 0), vec3(0, 0, 0)
function fixedUpdate(node, dt)
  n = n + 1
  local tyre, mount, p = find("Tyre"), find("Mount"), find("Planet")
  if not (tyre and mount and p) then return end
  -- The mount's place this tick is after this tick's carry.
  if n == 5 then pos = mount.tickPos + vec3(2, 0, 0) return end
  if pos then
    local c = tyre.carry
    -- What the frame they share carried, as the mount reads it.
    carried = carried + mount.carry
    if {use} then pos = pos + c end
    tyre.pos = pos
    if n % 30 == 0 then
      local d = tyre.tickPos - mount.tickPos - vec3(2, 0, 0)
      log(string.format("OFF %.4f %.4f %d", d:length(), carried:length(), n))
    end
  end
end
"#,
        use = use_carry
    );
    let (mut ed, dir) = project(if use_carry { "carryon" } else { "carryoff" }, &script);
    let star = ed.world.query::<Name>().find(|(_, n)| n.0 == "Star").map(|(e, _)| e).unwrap();
    // 280 m/s on its rails: v = sqrt(mu / a).
    ed.world.get_mut::<CelestialBody>(star).unwrap().mu = 280.0f64 * 280.0 * 1000.0;
    let planet = ed.world.spawn();
    ed.world.insert(planet, Transform::IDENTITY);
    ed.world.insert(planet, Name("Planet".into()));
    ed.world.insert(planet, Matter::Empty);
    ed.world.insert(planet, CelestialBody { mu: 0.0, body_radius: 5.0, soi: 500.0, parent: "Star".into(), a: 1000.0, ..Default::default() });
    for (name, kinematic) in [("Mount", false), ("Tyre", true)] {
        let e = ed.world.spawn();
        ed.world.insert(e, Transform::IDENTITY);
        ed.world.insert(e, Name(name.into()));
        ed.world.insert(e, Matter::Empty);
        let mode = if kinematic { floptle_core::BodyMode::Kinematic } else { floptle_core::BodyMode::Dynamic };
        ed.world.insert(e, RigidBody { mode, gravity: false, radius: 0.3, ..Default::default() });
    }
    ed.toggle_play();
    run(&mut ed, 1);
    let p = floptle_core::world_transform(&ed.world, planet).translation;
    ed.toggle_play();
    ed.world.get_mut::<Transform>(planet).unwrap().translation = p;
    for (name, off) in [("Mount", 20.0), ("Tyre", 22.0)] {
        let e = ed.world.query::<Name>().find(|(_, n)| n.0 == name).map(|(e, _)| e).unwrap();
        ed.world.get_mut::<Transform>(e).unwrap().translation = p + DVec3::new(off, 0.0, 0.0);
    }
    // The camera rides the planet, so the sim keeps recentring on it.
    let cam = ed.world.query::<Name>().find(|(_, n)| n.0 == "Camera").map(|(e, _)| e).unwrap();
    ed.world.insert(cam, floptle_core::Parent(planet));
    ed.world.get_mut::<Transform>(cam).unwrap().translation = DVec3::new(20.0, 2.0, 10.0);
    ed.toggle_play();
    // Recentring every half kilometre: several over the run.
    ed.sim.as_mut().unwrap().set_origin_threshold(500.0);
    let origin0 = ed.sim.as_ref().unwrap().world.origin;
    run(&mut ed, 600);
    let recentred = ed.sim.as_ref().unwrap().world.origin != origin0;
    assert!(recentred, "the sim never recentred, so this test says nothing about recentres");
    let off = lines(&mut ed, "OFF");
    let last: Vec<f64> = off.last().unwrap().split(' ').take(2).map(|v| v.parse().unwrap()).collect();
    let _ = std::fs::remove_dir_all(&dir);
    (last[0], last[1])
}

#[test]
fn node_carry_keeps_a_script_positioned_tyre_on_its_mount() {
    let (with, carried) = tyre_offset_after(true);
    assert!(carried > 100.0, "the planet should have carried the pair a long way: {carried} m");
    assert!(with < 0.01, "adding node.carry, the tyre is {with} m off its mount");
    let (without, carried) = tyre_offset_after(false);
    assert!((without - carried).abs() < 0.01, "without it the tyre should fall behind by the carry, {carried} m; it is {without} m off");
}

/// `node.carry` in `update` is what the ticks since the last `update`
/// carried, and `assembly.info(id).carry` is a craft's for this tick.
#[test]
fn node_carry_in_update_sums_the_frames_ticks() {
    let (mut ed, dir) = project(
        "carryframe",
        r#"
local ticks, n, ship = vec3(0, 0, 0), 0, nil
function fixedUpdate(node, dt)
  n = n + 1
  local b = find("Ball")
  if b then ticks = ticks + b.carry end
  local p = find("Planet")
  if n == 2 and p then
    spawn("Vessel", vec3(p.x, p.y + 10, p.z), function(v)
      spawn("Part", vec3(p.x, p.y + 10, p.z), function(q) assembly.rebuild(v, function(s) ship = s end) end, v)
    end)
  end
  if ship and n % 7 == 0 then
    local i = assembly.info(ship)
    log(string.format("SHIP %.4f %.4f %d", i.carry.x - b.carry.x, math.sqrt(i.carry.x ^ 2 + i.carry.y ^ 2 + i.carry.z ^ 2), n))
  end
end
function update(node, dt)
  local b = find("Ball")
  if b and n > 10 then
    log(string.format("FRAME %.4f %.4f %d", (b.carry - ticks):length(), b.carry:length(), n))
  end
  ticks = vec3(0, 0, 0)
end
"#,
    );
    let star = ed.world.query::<Name>().find(|(_, n)| n.0 == "Star").map(|(e, _)| e).unwrap();
    ed.world.get_mut::<CelestialBody>(star).unwrap().mu = 1.0e7;
    let planet = ed.world.spawn();
    ed.world.insert(planet, Transform::IDENTITY);
    ed.world.insert(planet, Name("Planet".into()));
    ed.world.insert(planet, Matter::Empty);
    ed.world.insert(planet, CelestialBody { mu: 0.0, body_radius: 5.0, soi: 500.0, parent: "Star".into(), a: 1000.0, ..Default::default() });
    let ball = ed.world.spawn();
    ed.world.insert(ball, Transform::IDENTITY);
    ed.world.insert(ball, Name("Ball".into()));
    ed.world.insert(ball, Matter::Empty);
    ed.world.insert(ball, RigidBody { gravity: false, radius: 0.3, ..Default::default() });
    ed.toggle_play();
    run(&mut ed, 1);
    let p = floptle_core::world_transform(&ed.world, planet).translation;
    ed.toggle_play();
    ed.world.get_mut::<Transform>(planet).unwrap().translation = p;
    ed.world.get_mut::<Transform>(ball).unwrap().translation = p + DVec3::new(0.0, 20.0, 0.0);
    let cam = ed.world.query::<Name>().find(|(_, n)| n.0 == "Camera").map(|(e, _)| e).unwrap();
    ed.world.get_mut::<Transform>(cam).unwrap().translation = p + DVec3::new(0.0, 0.0, 30.0);
    ed.toggle_play();
    for _ in 0..20 {
        ed.play_step(3.0 / 60.0, true);
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    }
    let frames = lines(&mut ed, "FRAME");
    assert!(frames.len() > 10, "{frames:?}");
    for l in &frames {
        let v: Vec<f64> = l.split(' ').take(2).map(|x| x.parse().unwrap()).collect();
        assert!(v[0] < 1e-6 && v[1] > 1.0, "update's carry is not the frame's ticks: {frames:?}");
    }
    let ship = lines(&mut ed, "SHIP");
    assert!(!ship.is_empty(), "{ship:?}");
    for l in &ship {
        let v: Vec<f64> = l.split(' ').take(2).map(|x| x.parse().unwrap()).collect();
        assert!(v[0].abs() < 1e-6 && v[1] > 0.1, "the craft's carry is not its frame's: {ship:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// `space.belt` makes its rocks: named, each a celestial on its row's orbit
/// about the parent, on the rails from the next tick, and the same rocks
/// from the same seed.
#[test]
fn a_belt_makes_named_rocks_on_their_orbits() {
    let (mut ed, dir) = project(
        "belt",
        r#"
local n, made = 0, 0
function update(node, dt)
  n = n + 1
  if n == 2 then
    space.belt{ parent = "Star", count = 20, seed = 3, aMin = 900, aMax = 1100, radiusMin = 2, radiusMax = 6,
      made = function(rock, i, row) made = made + 1 end }
  end
  if n == 30 then
    local s = find("Star")
    for i = 1, 20 do
      local r = find("Rock " .. i)
      if not r then log("MISSING " .. i) else
        local c = r:getcomponent("CelestialBody")
        local d = math.sqrt((r.x - s.x) ^ 2 + (r.y - s.y) ^ 2 + (r.z - s.z) ^ 2)
        log(string.format("ROCK %d %.3f %.3f %.3f", i, c.a, c.bodyRadius, d))
      end
    end
    log("MADE " .. made)
  end
end
"#,
    );
    ed.toggle_play();
    run(&mut ed, 32);
    assert!(lines(&mut ed, "MISSING").is_empty(), "rocks went missing");
    assert_eq!(lines(&mut ed, "MADE"), vec!["20".to_string()]);
    let rocks = lines(&mut ed, "ROCK");
    assert_eq!(rocks.len(), 20, "{rocks:?}");
    let rows = floptle_script::belt_orbits(20, 3, (900.0, 1100.0), 0.05, 0.05, (2.0, 6.0));
    for (k, l) in rocks.iter().enumerate() {
        let v: Vec<f64> = l.split(' ').skip(1).map(|x| x.parse().unwrap()).collect();
        assert!((v[0] - rows[k][0]).abs() < 1e-3 && (v[1] - rows[k][6]).abs() < 1e-3, "rock {k} is not its seed's row: {l}");
        // On its orbit about the star: within the orbit's eccentricity of a.
        assert!(v[2] > v[0] * 0.94 && v[2] < v[0] * 1.06, "rock {k} is {} from the star on an orbit of {}", v[2], v[0]);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A belt with a `shape` gives every rock ground of its own: an irregular
/// small body, made from the belt's seed when something comes near.** Each
/// rock is a terrain with its own id and a recipe naming its shape; "mixed"
/// deals all three shapes out across the belt; and the same seed deals them
/// the same way.
#[test]
fn a_belt_with_a_shape_gives_each_rock_its_ground() {
    use floptle_field::procgen::{PlanetFill, SmallBodyShape};
    let script = r#"
local n = 0
function update(node, dt)
  n = n + 1
  if n == 2 then
    space.belt{ parent = "Star", count = 12, seed = 4, aMin = 900, aMax = 1100, radiusMin = 4, radiusMax = 8,
      shape = "mixed", terrainId = 500 }
  end
end
"#;
    let dealt = || {
        let (mut ed, dir) = project("beltshape", script);
        ed.toggle_play();
        run(&mut ed, 6);
        let mut rocks: Vec<(u32, SmallBodyShape, f32)> = ed
            .world
            .query::<floptle_core::TerrainGen>()
            .filter_map(|(e, g)| {
                let Some(floptle_core::Matter::Terrain { id, .. }) = ed.world.get::<floptle_core::Matter>(e) else { return None };
                let fill: PlanetFill = ron::from_str(&g.0).ok()?;
                Some((*id, fill.small_body?.shape, fill.radius))
            })
            .collect();
        rocks.sort_by_key(|r| r.0);
        let _ = std::fs::remove_dir_all(&dir);
        rocks
    };
    let rocks = dealt();
    assert_eq!(rocks.iter().map(|r| r.0).collect::<Vec<_>>(), (500..512).collect::<Vec<u32>>(), "{rocks:?}");
    for shape in [SmallBodyShape::Potato, SmallBodyShape::ContactBinary, SmallBodyShape::Rubble] {
        assert!(rocks.iter().any(|r| r.1 == shape), "no {shape:?} in a mixed belt: {rocks:?}");
    }
    assert!(rocks.iter().all(|r| (4.0..=8.0).contains(&r.2)), "a rock is not its row's size: {rocks:?}");
    assert_eq!(rocks, dealt(), "the same seed dealt the shapes another way");
}

/// **A belt rock the camera comes up to gets its ground, with no script
/// call.** During Play only dynamic bodies anchored which terrain streamed
/// in, so a rock the camera flew up to with no body beside it never loaded:
/// it drew as a plain ball and `terrain.query` at its centre read empty
/// space, until a game called `terrain.warm`. The camera counts now. A rock
/// with the camera three radii off it, and no body anywhere, has ground at
/// its centre within a few seconds.
#[test]
fn a_belt_rock_the_camera_comes_up_to_gets_its_ground() {
    let script = r#"
local n = 0
function update(node, dt)
  n = n + 1
  if n == 2 then
    space.belt{ parent = "Star", count = 1, seed = 4, aMin = 1000, aMax = 1000, radiusMin = 8, radiusMax = 8,
      shape = "potato", terrainId = 500 }
  end
end
"#;
    let (mut ed, dir) = project("beltground", script);
    ed.toggle_play();
    run(&mut ed, 3);
    let rock = ed
        .world
        .query::<Matter>()
        .find_map(|(e, m)| matches!(m, Matter::Terrain { id: 500, .. }).then_some(e))
        .expect("the belt spawned no rock");
    assert!(ed.world.query::<RigidBody>().next().is_none(), "a body in the scene would anchor the rock's ground");
    assert!(!ed.terrains.contains_key(&rock), "the rock should start with no ground for this test to mean anything");
    let cam = ed.world.query::<Name>().find_map(|(e, n)| (n.0 == "Camera").then_some(e)).unwrap();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let solid = |ed: &crate::Editor| {
        let c = floptle_core::world_transform(&ed.world, rock).translation;
        ed.terrains.get(&rock).is_some_and(|t| t.field.d(ed.terrain_world_to_local(rock, c)) < 0.0)
    };
    while !solid(&ed) {
        // Three radii off the rock, following it round its orbit.
        let c = floptle_core::world_transform(&ed.world, rock).translation;
        ed.world.get_mut::<Transform>(cam).unwrap().translation = c + DVec3::new(0.0, 0.0, 24.0);
        ed.play_step(1.0 / 60.0, true);
        ed.pump_world_streaming();
        assert!(std::time::Instant::now() < until, "the rock never got its ground (resident: {})", ed.terrains.contains_key(&rock));
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// `destroy` on the root `assembly.split` made takes it and the parts under
/// it. The bare root the engine builds carried a body and a name but no
/// Matter, and the destroy lookup asked for Matter, so it was skipped with
/// nothing in the log. A second destroy of the same, now stale, handle says so.
#[test]
fn a_root_made_by_split_is_destroyed_with_its_parts() {
    let (mut ed, dir) = project(
        "split_destroy",
        r#"
local ship, parts, frame, stage = nil, {}, 0, nil
function update(node, dt)
  frame = frame + 1
  if frame == 1 then
    spawn("Vessel", vec3(0, 0, 0), function(v)
      ship = v
      for i = 1, 4 do
        spawn("Part", vec3(i - 2.5, 0, 0), function(p)
          parts[#parts + 1] = p
          if #parts == 4 then
            assembly.rebuild(v, function(s) log("BUILT ok") end)
          end
        end, v)
      end
    end)
  end
  if frame == 10 then
    assembly.split(ship, { parts[3], parts[4] }, function(r)
      stage = r
      log(string.format("SPLIT %d %d %d", r.__id, parts[3].__id, parts[4].__id))
      destroy(r)
      -- Already under the root this pass takes: gone as asked, not stale.
      destroy(parts[4])
    end)
  end
  if frame == 15 and stage then destroy(stage) end
end
"#,
    );
    ed.toggle_play();
    run(&mut ed, 6);
    assert_eq!(lines(&mut ed, "BUILT").len(), 1, "the four-part vessel was built");
    let before = ed.world.query::<Transform>().count();
    run(&mut ed, 8);
    let split = lines(&mut ed, "SPLIT");
    assert_eq!(split.len(), 1, "the split ran its callback: {split:?}");
    let ids: Vec<u32> = split[0].split(' ').map(|v| v.parse().unwrap()).collect();
    for id in &ids {
        assert!(ed.world.entity_at(*id).is_none(), "node #{id} outlived destroy (ids {ids:?})");
    }
    assert_eq!(
        ed.world.query::<Transform>().count(),
        before - 2,
        "the split root came and went, and took its two parts with it"
    );
    assert!(
        !ed.console.entries.iter().any(|e| e.msg.starts_with("destroy:")),
        "a destroy that worked is quiet"
    );
    run(&mut ed, 4);
    let stale = format!("destroy: node #{} no longer exists", ids[0]);
    assert!(
        ed.console.entries.iter().any(|e| e.msg.starts_with(&stale)),
        "a destroy of a stale handle says so: {:?}",
        ed.console.entries.iter().map(|e| &e.msg).collect::<Vec<_>>()
    );
    // Any node goes, whatever it carries: one with no Matter at all, and the
    // child under it.
    let bare = ed.world.spawn();
    ed.world.insert(bare, Transform::IDENTITY);
    ed.world.insert(bare, Name("Bare".into()));
    let kid = ed.world.spawn();
    ed.world.insert(kid, Transform::IDENTITY);
    ed.world.insert(kid, floptle_core::Parent(bare));
    ed.apply_destroys(vec![bare.index()]);
    assert!(!ed.world.is_alive(bare) && !ed.world.is_alive(kid), "a node with no Matter outlived destroy");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A thousand parts under one vessel, asked for in one frame, with or
/// without a spawn budget: the frame each frame took, the frames it took, and
/// what the script heard.
fn build_a_thousand(budget: Option<f64>) -> (Vec<f64>, Vec<String>) {
    let set = budget.map_or(String::new(), |ms| format!("spawnBudget({ms})"));
    let (mut ed, dir) = project(
        &format!("spawnk{}", budget.is_some()),
        &format!(
            r#"
local asked, frame = false, 0
function update(node, dt)
  frame = frame + 1
  -- The first frames pay for starting Play; the build starts after them.
  if frame < 3 then return end
  if asked then log("P " .. spawnPending()) return end
  asked = true
  {set}
  spawn("Vessel", vec3(0, 0, 0), function(v)
    for i = 1, 1000 do
      spawn("Part", vec3(i % 10, math.floor(i / 10) % 10, math.floor(i / 100)), function(p) p.y = p.y + 0.5 end, v)
    end
  end)
  spawnDone(function() log("DONE " .. frame .. " " .. spawnPending()) end)
end
"#
        ),
    );
    ed.toggle_play();
    let mut ms = Vec::new();
    run(&mut ed, 2);
    for _ in 0..2000 {
        let t0 = floptle_core::time::Instant::now();
        ed.play_step(1.0 / 60.0, true);
        ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
        if lines(&mut ed, "DONE").len() == 1 {
            break;
        }
    }
    let parts = ed.world.query::<Name>().filter(|(_, n)| n.0 == "Part").count();
    assert_eq!(parts, 1000, "the parts did not all spawn");
    ed.drain_script_logs();
    let said: Vec<String> = ed.console.entries.iter().map(|e| e.msg.clone()).filter(|m| m.starts_with("DONE") || m.starts_with("P ")).collect();
    let _ = std::fs::remove_dir_all(&dir);
    (ms, said)
}

/// **A spawn budget spreads a big build over frames, and says when it is
/// done.** A thousand parts asked for in one frame were one frame of every
/// spawn (145 to 320 ms in a real game). With `spawnBudget(2)` the frames
/// each spend about two milliseconds making parts, `spawnPending()` counts
/// down, and `spawnDone` hears once, with nothing pending, after the last.
/// A ratio, not a duration: the worst budgeted frame against the one frame
/// the same build takes without a budget.
#[test]
fn a_spawn_budget_spreads_a_big_build_and_says_when_it_is_done() {
    let (whole, _) = build_a_thousand(None);
    let one = whole.iter().copied().fold(0.0, f64::max);
    let (paced, said) = build_a_thousand(Some(2.0));
    let worst = paced.iter().copied().fold(0.0, f64::max);
    floptle_say::say!("SPAWN1000 one frame {one:.1} ms; budgeted worst {worst:.1} ms over {} frames {paced:.1?}", paced.len());
    let done: Vec<&String> = said.iter().filter(|l| l.starts_with("DONE")).collect();
    assert_eq!(done.len(), 1, "spawnDone heard {} times: {said:?}", done.len());
    assert!(done[0].ends_with(" 0"), "spawnDone heard with spawns still pending: {}", done[0]);
    assert!(paced.len() >= 4, "the budgeted build took {} frames", paced.len());
    let counts: Vec<usize> = said.iter().filter_map(|l| l.strip_prefix("P ")).filter_map(|n| n.parse().ok()).collect();
    assert!(counts.windows(2).all(|w| w[1] <= w[0]) && counts.first() > counts.last(), "spawnPending did not count down: {counts:?}");
    assert!(worst * 3.0 < one, "the worst budgeted frame took {worst:.1} ms against {one:.1} ms for the whole build in one");
}

/// **Ground that is not made yet does not read as open air.** A cold planet
/// has no collider at all, so `terrain.query` over it answers whatever else
/// is near and a ray straight down through it finds nothing. `terrain.ready`
/// says false there, and `terrain.raycast` answers "unknown" where it reaches
/// the body, until the field lands; then both read the real ground.
#[test]
fn ground_that_is_not_made_yet_reads_as_unknown_until_it_lands() {
    let (mut ed, dir) = project(
        "unmade",
        r#"
function update(node, dt)
  local rock, b = find("Rock"), find("Ball")
  if not (rock and b) then return end
  b.x, b.y, b.z = rock.x, rock.y + 30, rock.z
  local hit, why, at = terrain.raycast(vec3(rock.x, rock.y + 40, rock.z), vec3(0, -1, 0), 80)
  log(string.format("T %s %s %s", tostring(terrain.ready(rock.x, rock.y + 10, rock.z)),
    hit and string.format("hit %.2f", hit.distance) or why, tostring(at)))
end
"#,
    );
    let field = floptle_field::procgen::generate_planet(&floptle_field::procgen::PlanetFill {
        radius: 20.0,
        voxel: 1.5,
        relief: 2.0,
        cave_depth: 6.0,
        core_r: 3.0,
        seed: 1234,
        ..Default::default()
    });
    let path = ed.terrain_field_path_id(7);
    floptle_vfs::create_dir_all(path.parent().unwrap()).unwrap();
    floptle_vfs::write(&path, field.to_bytes()).unwrap();
    let rock = ed.world.spawn();
    ed.world.insert(rock, Transform::from_translation(DVec3::new(5000.0, -100_000.0, 0.0)));
    ed.world.insert(rock, Name("Rock".into()));
    ed.world.insert(rock, Matter::Terrain { id: 7, collision: Default::default() });
    ed.world.insert(rock, CelestialBody { mu: 0.0, body_radius: 20.0, parent: "Star".into(), a: 5000.0, ..Default::default() });
    let ball = ed.world.spawn();
    ed.world.insert(ball, Transform::IDENTITY);
    ed.world.insert(ball, Name("Ball".into()));
    ed.world.insert(ball, Matter::Empty);
    ed.world.insert(ball, RigidBody { gravity: false, radius: 0.5, ..Default::default() });
    ed.toggle_play();
    assert!(!ed.terrains.contains_key(&rock), "the planet should start cold for this test to mean anything");
    ed.play_step(1.0 / 60.0, true);
    for _ in 0..200 {
        ed.pump_world_streaming();
        ed.play_step(1.0 / 60.0, true);
        if ed.terrains.contains_key(&rock) && lines(&mut ed, "T").last().is_some_and(|l| l.starts_with("true")) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let said = lines(&mut ed, "T");
    assert!(ed.terrains.contains_key(&rock), "the planet never streamed in: {said:?}");
    let first = said.first().expect("the script never ran");
    assert!(first.starts_with("false unknown"), "before its field landed the planet read as {first:?}, not unknown");
    let last = said.last().unwrap();
    let d: f64 = last.strip_prefix("true hit ").and_then(|r| r.split(' ').next()).and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("after landing the ground should be ready and hit: {last:?}"));
    // Forty metres above the centre of a 20 m world with 2 m of relief.
    assert!((15.0..=25.0).contains(&d), "the ray met the ground {d} m down: {said:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **Stand-in ground says so, and `terrain.prepare` makes it real.** A world
/// that streams in makes the ground round the player exactly and holds the
/// rest as a coarse copy, so a query far from the player was off by up to
/// 0.75 m until that ground was made, with nothing to tell the game. Over the
/// made ground `terrain.ready` is true; over the stand-in it is false and a
/// raycast hit says `exact = false`; after `terrain.prepare` there, the
/// stand-in is made and both turn true.
#[test]
fn stand_in_ground_is_not_ready_until_it_is_prepared() {
    let (mut ed, dir) = project(
        "standin",
        r#"
local asked = false
function update(node, dt)
  local w = find("World")
  if not w then return end
  local top, side = vec3(w.x, w.y + 152, w.z), vec3(w.x + 152, w.y, w.z)
  if not asked then asked = true terrain.prepare(side.x, side.y, side.z, 4) end
  local hit = terrain.raycast(vec3(w.x + 200, w.y, w.z), vec3(-1, 0, 0), 100)
  log(string.format("S %s %s %s", tostring(terrain.ready(top.x, top.y, top.z)), tostring(terrain.ready(side.x, side.y, side.z)), tostring(hit and hit.exact)))
end
"#,
    );
    let fill = floptle_field::procgen::PlanetFill { seed: 8, radius: 150.0, voxel: 1.5, relief: 2.0, cave_depth: 0.0, ..Default::default() };
    let field = floptle_field::procgen::generate_planet_near(&fill, Vec3::new(0.0, 152.0, 0.0), floptle_field::CHUNK as f32 * fill.voxel);
    let at = DVec3::new(0.0, 0.0, -4000.0);
    let w = ed.world.spawn();
    ed.world.insert(w, Transform::from_translation(at));
    ed.world.insert(w, Name("World".into()));
    ed.world.insert(w, Matter::Terrain { id: 9, collision: Default::default() });
    ed.terrains.insert(w, crate::terrain_edit::EditorTerrain::new(field));
    ed.toggle_play();
    run(&mut ed, 2);
    let first = lines(&mut ed, "S");
    assert_eq!(first.first().map(String::as_str), Some("true false false"), "{first:?}");
    for _ in 0..300 {
        ed.pump_world_streaming();
        ed.play_step(1.0 / 60.0, true);
        if lines(&mut ed, "S").last().is_some_and(|l| l == "true true true") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let said = lines(&mut ed, "S");
    assert_eq!(said.last().map(String::as_str), Some("true true true"), "the prepared ground never became ready: {:?}", &said[said.len().saturating_sub(4)..]);
    let _ = std::fs::remove_dir_all(&dir);
}
