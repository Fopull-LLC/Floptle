//! Water with a swell, through the whole editor: a node's waves reach the
//! solver and the scripts on one clock, and the surface a shader draws is
//! the one a crate floats on.

use floptle_core::math::{DVec3, Vec3};
use floptle_core::water::Wave;
use floptle_core::{BodyKind, Matter, RigidBody, Transform, WaterKind};

/// A crate `half` metres from its middle to each face that floats half
/// under: its bounding sphere displaces twice its weight of water.
fn crate_body(half: f32) -> RigidBody {
    let r = Vec3::splat(half).length();
    let volume = 4.0 / 3.0 * std::f32::consts::PI * r * r * r;
    RigidBody { kind: BodyKind::Box, half_extents: [half; 3], mass: 500.0 * volume, rotates: true, ..Default::default() }
}

fn project(name: &str, files: &[(&str, &str)]) -> (crate::Editor, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("floptle-water-swell-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for (path, text) in files {
        let full = dir.join(path);
        floptle_vfs::create_dir_all(full.parent().unwrap()).unwrap();
        floptle_vfs::write(full, text).unwrap();
    }
    (crate::Editor { project_root: dir.clone(), ..Default::default() }, dir)
}

fn gravity(ed: &mut crate::Editor) {
    let g = ed.world.spawn();
    ed.world.insert(g, Transform::IDENTITY);
    ed.world.insert(g, Matter::GravityVolume { mode: floptle_core::GravityMode::Down, strength: 9.81, radius: 1.0e6 });
}

/// **A Water Volume's waves float a crate and answer a script, on one
/// clock.** A pool node with a 1 m roller: the crate on it rides the
/// formula's surface at the tick's `simTime`, `water.depthAt` in
/// `fixedUpdate` is the formula at that same `simTime`, and `water.setSwell`
/// with nil stills the node's water for the solver and the scene alike.
#[test]
fn a_nodes_swell_floats_a_crate_and_answers_scripts_on_the_sim_clock() {
    let (mut ed, dir) = project(
        "node",
        &[(
            "scripts/rider.lua",
            r#"worstAt, worstFloat, n, still = 0, 0, 0, false
local k = 2 * math.pi / 60
local omega = math.sqrt(9.81 * k)
function fixedUpdate(node, dt)
  if still then
    water.setSwell(water.volumes()[1], nil)
    return
  end
  local p = node.pos
  local h = math.sin(k * p.x - omega * simTime)
  worstAt = math.max(worstAt, math.abs(water.depthAt(p.x, -5, p.z) - (5 + h)))
  if simTime > 5 then
    worstFloat = math.max(worstFloat, math.abs(p.y - h))
    n = n + 1
  end
end
"#,
        )],
    );
    gravity(&mut ed);
    let pool = ed.world.spawn();
    ed.world.insert(pool, Transform::from_translation(DVec3::new(0.0, -20.0, 0.0)));
    let mut water = Matter::default_water();
    if let Matter::WaterVolume { kind, half_extents, waves, .. } = &mut water {
        *kind = WaterKind::Pool;
        *half_extents = [200.0, 20.0, 200.0];
        *waves = vec![Wave { amplitude: 1.0, wavelength: 60.0, direction: [1.0, 0.0], steepness: 0.0, speed: None, phase: 0.0 }];
    }
    ed.world.insert(pool, water);
    let rider = ed.world.spawn();
    ed.world.insert(rider, Transform::IDENTITY);
    ed.world.insert(rider, crate_body(0.5));
    ed.world.insert(rider, floptle_core::Scripts(vec![floptle_core::ScriptInst::new("rider")]));
    ed.toggle_play();
    for _ in 0..900 {
        ed.play_step(1.0 / 60.0, true);
    }
    assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    let env = ed.script_host.instance_env(rider.index(), "rider").unwrap();
    let num = |key: &str| env.get::<f64>(key).unwrap();
    assert!(num("n") > 500.0, "the fixture: ten seconds past the settle ({} ticks)", num("n"));
    assert!(num("worstAt") < 1e-3, "water.depthAt was {} m off the formula at simTime", num("worstAt"));
    assert!(num("worstFloat") < 0.3, "the crate was {:.2} m off the surface it floats on", num("worstFloat"));
    assert!(ed.sim.as_ref().unwrap().world.water.has_swell());

    env.set("still", true).unwrap();
    for _ in 0..3 {
        ed.play_step(1.0 / 60.0, true);
    }
    assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    assert!(matches!(ed.world.get::<Matter>(pool), Some(Matter::WaterVolume { waves, .. }) if waves.is_empty()), "the node kept its waves");
    assert!(!ed.sim.as_ref().unwrap().world.water.has_swell(), "the solver kept the swell");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A Water Volume with no waves is built as it always was.** No swell in
/// the solver's volume, none for scripts, and a shader's block for a node
/// on it is the still one.
#[test]
fn a_node_with_no_waves_has_no_swell_anywhere() {
    let (mut ed, dir) = project("none", &[]);
    let pool = ed.world.spawn();
    ed.world.insert(pool, Transform::IDENTITY);
    ed.world.insert(pool, Matter::default_water());
    let field = crate::Editor::build_water_field(&ed.world, DVec3::ZERO);
    assert!(field.volumes[0].swell.is_none() && !field.has_swell());
    assert!(crate::shading::water_swells(&ed.world).is_empty());
    assert_eq!(ed.swell_lanes_for(pool), crate::water_swell::STILL);
    // With waves, each of the three has them.
    if let Some(Matter::WaterVolume { waves, .. }) = ed.world.get_mut::<Matter>(pool) {
        waves.push(Wave::default());
    }
    assert!(crate::Editor::build_water_field(&ed.world, DVec3::ZERO).has_swell());
    assert_eq!(crate::shading::water_swells(&ed.world).len(), 1);
    let lanes = ed.swell_lanes_for(pool);
    assert_eq!(lanes[0][0], 1.0, "one wave");
    // A child of the water reads its swell; a node elsewhere reads none.
    let sheet = ed.world.spawn();
    ed.world.insert(sheet, Transform::from_translation(DVec3::new(3.0, 0.0, 0.0)));
    ed.world.insert(sheet, floptle_core::Parent(pool));
    assert_eq!(ed.swell_lanes_for(sheet)[0][0], 1.0);
    assert_eq!(ed.swell_lanes_for(sheet)[1][3], 3.0, "the sheet is 3 m along the water's x");
    let apart = ed.world.spawn();
    ed.world.insert(apart, Transform::IDENTITY);
    assert_eq!(ed.swell_lanes_for(apart), crate::water_swell::STILL);
    let _ = std::fs::remove_dir_all(&dir);
}

/// **The sea that is drawn is the sea a crate floats in.** Declared water
/// with a swell, its still surface from `water.surface` wearing a shader
/// that moves it with `waterSwell`, and crates floating in it: in the
/// picture each crate's top stands clear of the water and its keel is under
/// it, on a crest and in a trough alike. A sheet drawn flat would bare the
/// keel of a crate on a crest and drown one in a trough.
///
/// `FLOPTLE_SWELL_PNG=<path>` writes the picture.
#[test]
#[cfg(feature = "editor-ui")]
fn a_crate_sits_in_the_swell_that_is_drawn() {
    let Some(gpu_ed) = crate::offscreen::test_editor_with_gpu() else { return };
    let (_, dir) = project(
        "drawn",
        &[
            (
                "scripts/sea.lua",
                r#"local levels = {}
for i = 1, 121 * 121 do levels[i] = 0 end
sea = water.addVolume{ shape = "heightfield", origin = vec3(-60, 0, -60), cell = 1, columns = 121, rows = 121, levels = levels,
  waves = {
    { amplitude = 0.9,  wavelength = 24,  direction = { 1, 0.2 },  steepness = 0.8 },
    { amplitude = 0.35, wavelength = 9,   direction = { 0.3, 1 },  steepness = 0.7, phase = 1.3 },
    { amplitude = 0.15, wavelength = 4.5, direction = { -0.6, 1 }, steepness = 0.6, phase = 4.0 },
  } }
function start(node)
  node.model = mesh.build(water.surface(sea))
  node:setMaterial{ shader = "shaders/sea.flsl" }
end
"#,
            ),
            (
                "shaders/sea.flsl",
                r#"shader sea {
  stage fragment
  let n = waterSwellNormal(objectPos)
  let sun = max(dot(n, normalize(vec3(0.5, 0.7, 0.3))), 0)
  let crest = smoothstep(0.2, 1.1, waterSwell(objectPos).y)
  output color = vec4(mix(vec3(0.01, 0.10, 0.22), vec3(0.15, 0.5, 0.7), sun * sun) + vec3(crest * 0.3), 1)
  output offset = waterSwell(objectPos)
}
"#,
            ),
        ],
    );
    let mut ed = gpu_ed;
    ed.project_root = dir.clone();
    gravity(&mut ed);
    let sun = ed.world.spawn();
    ed.world.insert(sun, Transform::IDENTITY);
    ed.world.insert(sun, floptle_core::Light::default());
    let sea = ed.world.spawn();
    ed.world.insert(sea, Transform::IDENTITY);
    ed.world.insert(sea, floptle_core::Scripts(vec![floptle_core::ScriptInst::new("sea")]));
    let crates: Vec<_> = [(-7.0, 2.0), (0.0, 0.0), (6.0, -3.0), (11.0, 4.0)]
        .iter()
        .map(|&(x, z)| {
            let e = ed.world.spawn();
            ed.world.insert(e, Transform::from_translation(DVec3::new(x, 0.3, z)));
            ed.world.insert(e, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [1.0, 0.1, 0.05] });
            // The size a Cube primitive is drawn.
            ed.world.insert(e, crate_body(crate::matter_catalog::PRIMITIVE_HALF));
            e
        })
        .collect();
    ed.toggle_play();
    // Let them settle, then go on until one is well up a crest and another
    // well down a trough.
    let wave_at = |ed: &crate::Editor, e: floptle_core::Entity| {
        let p = floptle_core::world_transform(&ed.world, e).translation;
        ed.script_host.declared_water()[0].at(DVec3::new(p.x, -50.0, p.z)).wave
    };
    let mut ticks = 0;
    loop {
        ed.play_step(1.0 / 60.0, true);
        ticks += 1;
        let waves: Vec<f64> = crates.iter().map(|&e| wave_at(&ed, e)).collect();
        if ticks > 300 && waves.iter().any(|h| *h > 0.6) && waves.iter().any(|h| *h < -0.6) {
            break;
        }
        assert!(ticks < 3000, "no crate ever rode a crest while another sat in a trough: {waves:?}");
    }
    assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());

    let (w, h) = (960u32, 540u32);
    let cam = floptle_render::RenderCamera::new(
        DVec3::new(2.0, 9.0, 22.0),
        floptle_core::math::Quat::from_rotation_x(-0.38),
        floptle_render::Projection::Perspective { fov_y: 45f32.to_radians(), near: 0.1, far: 500.0 },
    );
    let px = crate::shot::render_frame_pixels(&mut ed, &cam, None, w, h, u32::MAX, false).expect("no device");
    if let Ok(path) = std::env::var("FLOPTLE_SWELL_PNG") {
        floptle_image::io::save_png(std::path::Path::new(&path), &px, w, h).unwrap();
    }
    let view_proj = cam.view_proj(w as f32 / h as f32);
    let red_at = |p: DVec3| {
        let clip = view_proj * (p - cam.world_position).as_vec3().extend(1.0);
        let (x, y) = ((clip.x / clip.w * 0.5 + 0.5) * w as f32, (0.5 - clip.y / clip.w * 0.5) * h as f32);
        let o = ((y as u32).min(h - 1) * w + (x as u32).min(w - 1)) as usize * 4;
        px[o] > 40 && px[o] as u32 > 2 * px[o + 1] as u32 && px[o] as u32 > 2 * px[o + 2] as u32
    };
    let mut seen = 0;
    for &e in &crates {
        let t = floptle_core::world_transform(&ed.world, e);
        let up = t.rotation.as_dquat() * DVec3::Y;
        let (wave, lift) = (wave_at(&ed, e), up.y.abs());
        let above = ed.script_host.declared_water()[0].at(t.translation).above;
        assert!(above.abs() < 0.35, "the crate at {:?} floats {above:+.2} m from the surface of a {wave:+.2} m wave", t.translation);
        // Its own top and bottom faces, a little in from each.
        let (top, keel) = if up.y >= 0.0 { (t.translation + up * 0.55, t.translation - up * 0.55) } else { (t.translation - up * 0.55, t.translation + up * 0.55) };
        if lift < 0.8 {
            continue; // rolled onto an edge: neither face is plainly up
        }
        assert!(red_at(top), "the crate at {:?} on a {wave:+.2} m wave is not drawn above the water", t.translation);
        assert!(!red_at(keel), "the crate at {:?} on a {wave:+.2} m wave shows its keel: the drawn water is not where it floats", t.translation);
        seen += 1;
    }
    assert!(seen >= 3, "the fixture: only {seen} of the crates sat upright enough to judge");
    let _ = std::fs::remove_dir_all(&dir);
}
