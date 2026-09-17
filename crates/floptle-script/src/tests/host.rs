use super::*;

#[test]
fn rotate_script_drives_yaw() {
    let dir = std::env::temp_dir().join("floptle_script_test_rotate");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "rotate",
        "defaults = { speed = 90 }\nfunction update(node, dt)\n  node.yaw = node.yaw + math.rad(params.speed) * dt\nend\n",
    );

    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, Scripts(vec![floptle_core::ScriptInst {
        kind: "rotate".into(),
        enabled: true,
        params: vec![("speed".into(), 90.0)], refs: Vec::new(),
        strs: Vec::new(),
    }]));

    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0, 1.0); // 90 deg/s for 1s -> ~pi/2 yaw
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let tr = world.get::<Transform>(e).unwrap();
    let (yaw, _, _) = tr.rotation.to_euler(EulerRot::YXZ);
    assert!((yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-3, "yaw was {yaw}");
}

#[test]
fn script_can_draw_gizmos() {
    let dir = std::env::temp_dir().join("floptle_script_test_gizmos");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "drawer",
        "function update(node, dt)\n  gizmo.line(0,0,0, 1,2,3)\n  gizmo.ray(0,0,0, 0,-2,0, 5, 1,0,0)\n  gizmo.sphere(4,5,6, 2)\n  gizmo.point(7,8,9)\nend\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst { kind: "drawer".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let cmds = host.take_gizmos();
    assert_eq!(cmds.len(), 4);
    // Explicit color sticks; ray normalizes the direction and scales by len.
    match cmds[1] {
        GizmoCmd::Line { a, b, color } => {
            assert_eq!(a, [0.0, 0.0, 0.0]);
            assert!((b[1] + 5.0).abs() < 1e-4, "ray end {b:?}");
            assert_eq!(color, [1.0, 0.0, 0.0]);
        }
        ref other => panic!("expected a line from gizmo.ray, got {other:?}"),
    }
    // Omitted color falls back to the default green.
    match cmds[0] {
        GizmoCmd::Line { color, .. } => assert!(color[1] > 0.9),
        ref other => panic!("expected a line, got {other:?}"),
    }
    // A second run() starts a fresh (empty) batch — immediate mode.
    host.run(&mut world, &dir, 0.1, 0.2);
    assert_eq!(host.take_gizmos().len(), 4);
}

#[test]
fn defaults_are_read() {
    let dir = std::env::temp_dir().join("floptle_script_test_defaults");
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "pulsate", "defaults = { amplitude = 0.3, speed = 2.0, base = 1.0 }\n");
    let host = ScriptHost::new();
    let (d, refs, _strs) = host.script_defaults(&dir.join("pulsate.lua"));
    assert_eq!(d.len(), 3);
    assert!(refs.is_empty());
    assert!(d.iter().any(|(k, v)| k == "amplitude" && (*v - 0.3).abs() < 1e-6));
}

/// **When only transforms moved, the mirror is refreshed, not rebuilt** —
/// and the refresh is real: a handle reads the moved value.
///
/// Three full rebuilds a frame were most of what a large scene cost the
/// script host outside the hooks. The guard is a count of rebuilds, and it
/// is watched in both directions with the rename test below.
#[test]
fn a_transform_only_change_refreshes_the_mirror_without_a_rebuild() {
    let dir = std::env::temp_dir().join(format!("floptle_mirror_refresh_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "reader", "function update(node, dt) node.y = find('Other').x end\n");
    let (mut world, e) = world_with_script("reader");
    let other = world.spawn();
    world.insert(other, Transform::IDENTITY);
    world.insert(other, floptle_core::Name("Other".into()));
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    crate::host::FULL_SYNCS.with(|c| c.set(0));

    // Physics-shaped change: a transform, through `get_mut`, nothing else.
    world.get_mut::<Transform>(other).unwrap().translation.x = 7.0;
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    host.run_fixed(&mut world, 1.0 / 60.0, 1.0 / 60.0);
    host.run_late(&mut world, 1.0 / 60.0, 1.0 / 60.0);
    assert_eq!(
        crate::host::FULL_SYNCS.with(|c| c.get()),
        0,
        "three passes over a transform-only change must not rebuild the mirror"
    );
    assert_eq!(
        world.get::<Transform>(e).unwrap().translation.y,
        7.0,
        "the refresh did not carry the moved transform to a handle"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn script_reads_and_writes_a_component_field() {
    // node:getcomponent("PointLight") reads the light's live fields, and assigning one
    // flushes back to the ECS the same frame.
    let dir = std::env::temp_dir().join("floptle_script_test_component");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "oscillate",
        "function update(node, dt)\n  local l = node:getcomponent(\"PointLight\")\n  if l then l.intensity = l.intensity + 1.0 end\nend\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, Matter::PointLight {
        color: [1.0, 1.0, 1.0],
        intensity: 2.0,
        range: 10.0,
        shape: Default::default(),
        shadows: false, spot_angle: floptle_core::OMNI_ANGLE, spot_softness: 0.25,
    });
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst { kind: "oscillate".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    match world.get::<Matter>(e).unwrap() {
        Matter::PointLight { intensity, .. } => {
            assert!((*intensity - 3.0).abs() < 1e-4, "intensity became {intensity}, expected 3.0")
        }
        other => panic!("expected point light, got {other:?}"),
    }
}

#[test]
fn layers_and_tags_round_trip_through_the_lua_api() {
    // node.layer reads "Default" when unset; a valid write lands as a
    // Layer component; tags edit read-your-writes and flush as Tags; a
    // findTagged scan sees a PRE-EXISTING tag the same frame.
    let dir = std::env::temp_dir().join("floptle_script_test_layers");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "layerer",
        "function update(node, dt)\n\
         local score = 0\n\
         if node.layer == \"Default\" then score = score + 1 end\n\
         node.layer = \"Enemies\"\n\
         if node.layer == \"Enemies\" then score = score + 10 end\n\
         node:addTag(\"boss\")\n\
         node:addTag(\"boss\")\n\
         if node:hasTag(\"boss\") and #node.tags == 1 then score = score + 100 end\n\
         if #findTagged(\"marked\") == 1 then score = score + 1000 end\n\
         local ok, err = pcall(function() node.layer = \"Typo\" end)\n\
         if not ok then score = score + 10000 end\n\
         node.x = score\n\
        end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst { kind: "layerer".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
    );
    let marked = world.spawn();
    world.insert(marked, Transform::IDENTITY);
    world.insert(marked, floptle_core::Tags(vec!["marked".into()]));
    let mut host = ScriptHost::new();
    host.set_layers(floptle_core::Layers::resolve(
        vec!["Default".into(), "Enemies".into()],
        &[],
    ));
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 11111.0);
    assert_eq!(
        world.get::<floptle_core::Layer>(e).map(|l| l.0.clone()),
        Some("Enemies".to_string())
    );
    assert_eq!(
        world.get::<floptle_core::Tags>(e).map(|t| t.0.clone()),
        Some(vec!["boss".to_string()])
    );
}

/// `spawn(prefab, pos, fn)` queues a request (with the position and the
/// callback), `destroy(node)` / `node:destroy()` queue entity indices, and
/// the driver-invoked callback configures the freshly spawned node.
#[test]
fn spawn_and_destroy_queue_and_callback_configures_the_new_node() {
    let dir = std::env::temp_dir().join("floptle_script_test_spawn");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "spawner",
        "function update(node, dt)\n\
           if not done then\n\
             done = true\n\
             spawn(\"bullet\", vec3(1, 2, 3), function(b)\n\
               b.x = 42\n\
             end)\n\
             destroy(node)\n\
             local victim = find(\"Victim\")\n\
             victim:destroy()\n\
           end\n\
         end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "spawner".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    world.insert(e, floptle_core::Matter::Empty);
    let victim = world.spawn();
    world.insert(victim, Transform::IDENTITY);
    world.insert(victim, floptle_core::Name("Victim".into()));
    world.insert(victim, floptle_core::Matter::Empty);
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.016, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());

    let mut spawns = host.take_spawn_requests();
    assert_eq!(spawns.len(), 1);
    let req = spawns.remove(0);
    assert_eq!(req.prefab, "bullet");
    assert_eq!(req.pos, Some([1.0, 2.0, 3.0]));
    let destroys = host.take_destroy_requests();
    assert_eq!(destroys, vec![e.index(), victim.index()], "both destroy forms queue");
    assert!(host.take_spawn_requests().is_empty(), "drain empties the queue");

    // The driver spawns the prefab (simulated here) and hands the callback
    // the new root — its writes flush straight to the ECS.
    let bullet = world.spawn();
    world.insert(bullet, Transform::IDENTITY);
    world.insert(bullet, floptle_core::Name("bullet".into()));
    world.insert(bullet, floptle_core::Matter::Empty);
    host.call_spawn_callback(
        &mut world,
        req.cb.expect("callback captured"),
        bullet.index(),
        &[bullet],
    );
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(world.get::<Transform>(bullet).unwrap().translation.x, 42.0);
}

#[test]
fn assets_api_resolves_under_project_root() {
    // assets.getFile returns the path for an existing file (nil for a missing one);
    // assets.getContents lists a directory. Encode the three results into node.x.
    let root = std::env::temp_dir().join("floptle_script_test_assets_root");
    let models = root.join("models");
    let _ = std::fs::create_dir_all(&models);
    let _ = std::fs::write(models.join("armor.glb"), b"x");
    let dir = std::env::temp_dir().join("floptle_script_test_assets_scripts");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "probe",
        "function update(node, dt)\n  local f = assets.getFile(\"models/armor.glb\")\n  local missing = assets.getFile(\"models/nope.glb\")\n  local c = assets.getContents(\"models\")\n  node.x = (f ~= nil and 1 or 0) + (missing == nil and 10 or 0) + (#c == 1 and 100 or 0)\nend\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst { kind: "probe".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
    );
    let mut host = ScriptHost::new();
    host.set_project_root(root);
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 111.0);
}

#[test]
fn save_api_round_trips_across_hosts() {
    // set → flush writes save/<slot>.ron; a FRESH host (a new play session /
    // process) reads the same values back. Tables survive; defaults fill gaps.
    let root = std::env::temp_dir().join("floptle_script_test_save_root");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::create_dir_all(&root);
    let dir = std::env::temp_dir().join("floptle_script_test_save_scripts");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "writer",
        "function update(node, dt)\n  save.set(\"gold\", 42)\n  save.set(\"who\", {name=\"Ty\", hp=7})\n  save.flush()\nend\n",
    );
    write_script(
        &dir,
        "reader",
        "function update(node, dt)\n  local who = save.get(\"who\")\n  node.x = save.get(\"gold\", 0) + (who and who.hp or 0) * 1000 + save.get(\"missing\", 5)\nend\n",
    );
    let run = |kind: &str| -> f64 {
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: kind.into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.set_project_root(root.clone());
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        world.get::<Transform>(e).unwrap().translation.x
    };
    run("writer");
    assert!(root.join("save/main.ron").exists(), "flush wrote the slot file");
    assert_eq!(run("reader"), 42.0 + 7000.0 + 5.0);
}
