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

/// A level of `n` plain named nodes plus a camera named `Cam`, and a script
/// node running `kind`.
fn level_with_camera(kind: &str, n: usize) -> (World, Entity, Entity) {
    let (mut world, e) = world_with_script(kind);
    for i in 0..n {
        let s = world.spawn();
        world.insert(s, Transform::IDENTITY);
        world.insert(s, floptle_core::Name(format!("Prop {i}")));
        world.insert(s, Visible(true));
    }
    let cam = world.spawn();
    world.insert(cam, Transform::IDENTITY);
    world.insert(cam, floptle_core::Name("Cam".into()));
    world.insert(
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
        },
    );
    (world, e, cam)
}

/// **A component value written every frame re-reads that one node, not the
/// scene.** A camera's FOV eased by speed, a light's shimmer, a vignette
/// pulse: each used to rebuild the whole mirror on every pass, which cost
/// milliseconds on a 1,361-node level and grew as the level grew, while the
/// profiler named no script. The write must still land, and a second script
/// reading it back must see the new value.
#[test]
fn a_component_write_every_frame_refreshes_one_node_not_the_whole_mirror() {
    let dir = std::env::temp_dir().join(format!("floptle_mirror_fov_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "zoomer",
        "t = 0\nfunction update(node, dt)\n  t = t + 1\n  find('Cam'):getComponent('Camera').fovY = 1 + t * 0.01\n  node.x = find('Cam2'):getComponent('Camera').fovY\nend\n",
    );
    let (mut world, e, cam) = level_with_camera("zoomer", 2000);
    // A second camera the script only reads, changed from outside the script:
    // the refreshed mirror has to serve the new value, not the one it held.
    let cam2 = world.spawn();
    world.insert(cam2, Transform::IDENTITY);
    world.insert(cam2, floptle_core::Name("Cam2".into()));
    let m = world.get::<Matter>(cam).unwrap().clone();
    world.insert(cam2, m);
    let mut host = ScriptHost::new();
    host.profile().borrow_mut().enable(true);
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    crate::host::FULL_SYNCS.with(|c| c.set(0));
    for f in 1..=5 {
        if let Some(Matter::Camera { fov_y, .. }) = world.get_mut::<Matter>(cam2) {
            *fov_y = 2.0 + f as f32 * 0.1;
        }
        host.run(&mut world, &dir, 1.0 / 60.0, f as f32 / 60.0);
        host.run_fixed(&mut world, 1.0 / 60.0, f as f32 / 60.0);
        host.run_late(&mut world, 1.0 / 60.0, f as f32 / 60.0);
        host.profile().borrow_mut().end_frame();
    }
    assert_eq!(
        crate::host::FULL_SYNCS.with(|c| c.get()),
        0,
        "a per-frame FOV write rebuilt the mirror (cause: {:?})",
        host.profile().borrow().mirror_cause()
    );
    let Some(Matter::Camera { fov_y, .. }) = world.get::<Matter>(cam) else { panic!("camera gone") };
    assert!((fov_y - 1.06).abs() < 1e-4, "the write did not land: fovY {fov_y}");
    let x = world.get::<Transform>(e).unwrap().translation.x;
    assert!((x - 2.5).abs() < 1e-4, "the mirror served a stale fovY: {x}");
    let work = host.profile().borrow().mirror_work();
    assert_eq!(work.rebuilds, 0);
    assert!(work.refreshed >= 1 && work.refreshed <= 8, "refreshed {} nodes", work.refreshed);
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A write of the value a node already has costs no rebuild.** Scripts
/// write `bar.visible = show` every frame, `show` usually unchanged.
#[test]
fn a_same_value_visible_write_every_frame_does_not_rebuild_the_mirror() {
    let dir = std::env::temp_dir().join(format!("floptle_mirror_vis_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "hider",
        "function update(node, dt)\n  local p = find('Prop 7')\n  p.visible = p.visible\nend\nfunction fixedUpdate(node, dt)\n  local p = find('Prop 9')\n  p.visible = false\nend\n",
    );
    let (mut world, _e, _cam) = level_with_camera("hider", 50);
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    crate::host::FULL_SYNCS.with(|c| c.set(0));
    for f in 1..=4 {
        host.run(&mut world, &dir, 1.0 / 60.0, f as f32 / 60.0);
        host.run_fixed(&mut world, 1.0 / 60.0, f as f32 / 60.0);
        host.run_late(&mut world, 1.0 / 60.0, f as f32 / 60.0);
    }
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(crate::host::FULL_SYNCS.with(|c| c.get()), 0, "a visibility write rebuilt the mirror");
    let p9 = world.query::<floptle_core::Name>().find(|(_, n)| n.0 == "Prop 9").unwrap().0;
    assert_eq!(world.get::<Visible>(p9).map(|v| v.0), Some(false), "the real write did not land");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A structural change still rebuilds, and says what it was.** The refresh
/// path must not swallow a rename: `find` has to see the new name.
#[test]
fn a_rebuild_names_the_node_and_the_change_that_caused_it() {
    let dir = std::env::temp_dir().join(format!("floptle_mirror_cause_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "idle", "function update(node, dt) end\n");
    let (mut world, _e, cam) = level_with_camera("idle", 3);
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    crate::host::FULL_SYNCS.with(|c| c.set(0));
    world.insert(cam, floptle_core::Tags(vec!["main".into()]));
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    assert_eq!(crate::host::FULL_SYNCS.with(|c| c.get()), 1, "a tag change must rebuild");
    let cause = host.profile().borrow().mirror_cause().unwrap_or_default().to_string();
    assert!(cause.contains("tags") && cause.contains("'Cam'"), "cause: {cause}");
    let _ = std::fs::remove_dir_all(&dir);
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
    // findTagged scan sees a pre-existing tag the same frame.
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
    // set → flush writes save/<slot>.ron; a fresh host (a new play session /
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

/// **A sound is preloaded by the audio side, a model by the model side, and
/// one callback waits for both.** `assets.preload` tells them apart by
/// extension; `audio.preload` takes the extensionless names `audio.play` does.
/// `sound:isLoading()` reads the mirror the driver feeds.
#[test]
fn a_preload_sends_sounds_to_the_audio_side_and_waits_for_both_kinds() {
    use crate::PreloadKind::{Model, Sound};
    let dir = std::env::temp_dir().join("floptle_script_test_preload_sounds");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "warm",
        "calls = 0\n\
         function start(node)\n\
           assets.preload({ \"models/arm.glb\", \"audio/music.OGG\" }, function(failed)\n\
             calls = calls + 1; failedCount = #failed\n\
           end)\n\
           audio.preload(\"audio/hit\")\n\
           music = audio.play(\"audio/music.OGG\")\n\
         end\n\
         function update(node, dt)\n\
           loading = music:isLoading(); playing = music:isPlaying()\n\
         end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "warm".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(host.take_preload_requests(Model), vec!["models/arm.glb".to_string()]);
    assert_eq!(
        host.take_preload_requests(Sound),
        vec!["audio/music.OGG".to_string(), "audio/hit".to_string()]
    );
    let env = |host: &ScriptHost| host.instance_env(e.index(), "warm").unwrap();

    // The model is in; the sound is not. Still waiting.
    host.set_preload_status(Model, HashMap::from([("models/arm.glb".to_string(), true)]));
    let mut info = crate::AudioInfo::default();
    info.sounds.insert(1, crate::AudioPlayState { playing: true, loading: true, ..Default::default() });
    host.set_audio_info(info);
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    assert_eq!(env(&host).get::<i64>("calls").unwrap(), 0, "called back before the sound had answered");
    assert!(env(&host).get::<bool>("loading").unwrap());
    assert!(env(&host).get::<bool>("playing").unwrap(), "a sound waiting for its clip read as stopped");

    // The sound answers; the model's answer, given a frame ago, still stands.
    host.set_preload_status(Sound, HashMap::from([("audio/music.OGG".to_string(), true)]));
    host.set_audio_info(crate::AudioInfo::default());
    host.run(&mut world, &dir, 1.0 / 60.0, 2.0 / 60.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(env(&host).get::<i64>("calls").unwrap(), 1);
    assert_eq!(env(&host).get::<i64>("failedCount").unwrap(), 0);
    assert!(!env(&host).get::<bool>("loading").unwrap());
}

/// **`assets.preload` waits for every model it named, and says which failed.**
/// The driver is asked to start the imports, reports as they land, and the
/// callback runs in the frame pass once all of them have answered: not when
/// the first one does.
#[test]
fn a_preload_calls_back_once_every_model_is_in() {
    let dir = std::env::temp_dir().join("floptle_script_test_preload");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "warm",
        "calls = 0\n\
         function start(node)\n\
           assets.preload({ \"models/arm.glb\", \"models/leg.glb\" }, function(failed)\n\
             calls = calls + 1; failedCount = #failed; firstFailed = failed[1]\n\
           end)\n\
         end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "warm".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(host.take_preload_requests(crate::PreloadKind::Model), vec!["models/arm.glb".to_string(), "models/leg.glb".to_string()]);
    let calls = |host: &ScriptHost| -> i64 { host.instance_env(e.index(), "warm").unwrap().get("calls").unwrap() };

    // One of two in: still waiting.
    host.set_preload_status(crate::PreloadKind::Model, HashMap::from([("models/arm.glb".to_string(), true)]));
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    assert_eq!(calls(&host), 0, "called back before every model had answered");

    host.set_preload_status(crate::PreloadKind::Model, HashMap::from([
        ("models/arm.glb".to_string(), true),
        ("models/leg.glb".to_string(), false),
    ]));
    host.run(&mut world, &dir, 1.0 / 60.0, 2.0 / 60.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let env = host.instance_env(e.index(), "warm").unwrap();
    assert_eq!(calls(&host), 1);
    assert_eq!(env.get::<i64>("failedCount").unwrap(), 1);
    assert_eq!(env.get::<String>("firstFailed").unwrap(), "models/leg.glb");
    assert!(host.preload_waiting_on(crate::PreloadKind::Model).is_empty(), "an answered preload must stop waiting");
    host.run(&mut world, &dir, 1.0 / 60.0, 3.0 / 60.0);
    assert_eq!(calls(&host), 1, "a preload calls back once");
}

/// **A polyline is the segments the per-segment calls would have made.** One
/// call per curve instead of one per segment, and the same vertices out.
#[test]
fn a_polyline_draws_the_same_segments_as_one_line_call_each() {
    let dir = std::env::temp_dir().join(format!("floptle_script_test_polyline_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "curves",
        "function update(node, dt)\n  \
           local flat, pts = {}, {}\n  \
           for i = 0, 127 do\n    \
             local t = i / 127 * 6.283\n    \
             local x, y, z = math.cos(t) * 5, i * 0.01, math.sin(t) * 5\n    \
             flat[#flat+1] = x; flat[#flat+1] = y; flat[#flat+1] = z\n    \
             pts[#pts+1] = vec3(x, y, z)\n  \
           end\n  \
           draw.polyline(flat, 1, 0.5, 0.25, 0.8)\n  \
           for i = 1, 127 do\n    \
             local a, b = (i - 1) * 3, i * 3\n    \
             draw.line(flat[a+1], flat[a+2], flat[a+3], flat[b+1], flat[b+2], flat[b+3], 1, 0.5, 0.25, 0.8)\n  \
           end\n  \
           draw.polyline(pts, 1, 0.5, 0.25, 0.8)\n\
         end\n",
    );
    let (mut world, _) = world_with_script("curves");
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    let lines = host.take_draw_lines();
    assert_eq!(lines.len(), 127 * 3, "one polyline of 128 points is 127 segments");
    let (poly, rest) = lines.split_at(127);
    let (singles, vecs) = rest.split_at(127);
    for i in 0..127 {
        assert_eq!((poly[i].a, poly[i].b, poly[i].color), (singles[i].a, singles[i].b, singles[i].color), "segment {i}");
        for k in 0..3 {
            assert!((vecs[i].a[k] - singles[i].a[k]).abs() < 1e-5 && (vecs[i].b[k] - singles[i].b[k]).abs() < 1e-5);
        }
    }
}

/// **A conic is the orbit it names.** Every vertex of an ellipse sits at
/// `r = p / (1 + e cos θ)` from the focus, the loop closes, and an open orbit
/// stops where `r` reaches `maxR`.
#[test]
fn a_conic_is_a_closed_ellipse_or_an_arc_cut_at_its_reach() {
    let dir = std::env::temp_dir().join(format!("floptle_script_test_conic_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "orbit",
        "function update(node, dt)\n  \
           draw.conic(vec3(10, 0, 0), vec3(1, 0, 0), vec3(0, 0, 1), 4, 0.5, 64, 1, 1, 1)\n  \
           draw.conic(vec3(0, 0, 0), vec3(1, 0, 0), vec3(0, 0, 1), 2, 1.5, 32, 1, 1, 1, 1, 20)\n\
         end\n",
    );
    let (mut world, _) = world_with_script("orbit");
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    let lines = host.take_draw_lines();
    assert_eq!(lines.len(), 64 + 32, "a closed ellipse of 64 segments and an open arc of 32");
    let focus = glam::DVec3::new(10.0, 0.0, 0.0);
    for l in &lines[..64] {
        let d = glam::DVec3::from(l.a) - focus;
        let th = d.z.atan2(d.x);
        let want = 4.0 / (1.0 + 0.5 * th.cos());
        assert!((d.length() - want).abs() < 1e-6, "vertex off the ellipse: {} vs {want}", d.length());
    }
    assert_eq!(lines[63].b, lines[0].a, "the ellipse closes on its first vertex");
    let ends = [glam::DVec3::from(lines[64].a).length(), glam::DVec3::from(lines[95].b).length()];
    for r in ends {
        assert!((r - 20.0).abs() < 1e-6, "the arc ends where r reaches maxR, got {r}");
    }
}

/// **A spawn or a despawn is not a rebuild, and says the same thing one
/// would.** After each round of random spawns and despawns the scripts' copy
/// of the scene is exactly what a fresh full rebuild makes of the same world
/// (the scene order, first-name-wins, the per-kind and per-tag lists, children)
/// and the incremental host never rebuilt.
#[test]
fn spawning_and_destroying_nodes_keeps_the_mirror_exact_without_a_rebuild() {
    fn node(world: &mut World, i: u32, parent: Option<Entity>) -> Entity {
        let e = world.spawn();
        world.insert(e, Transform::from_translation(glam::DVec3::new(i as f64, 0.0, 0.0)));
        if i.is_multiple_of(3) {
            world.insert(e, floptle_core::Name("Crate".into()));
        } else {
            world.insert(e, floptle_core::Name(format!("n{i}")));
        }
        if i.is_multiple_of(4) {
            world.insert(e, floptle_core::Tags(vec!["enemy".into(), format!("t{}", i % 7)]));
        }
        if i.is_multiple_of(5) {
            world.insert(e, Scripts(vec![floptle_core::ScriptInst::new("ai")]));
        }
        if let Some(p) = parent {
            world.insert(e, floptle_core::Parent(p));
        }
        e
    }
    fn snapshot(h: &ScriptHost) -> String {
        let s = h.scene.borrow();
        let sorted = |m: &HashMap<u32, String>| {
            let mut v: Vec<_> = m.iter().map(|(k, v)| format!("{k}={v}")).collect();
            v.sort();
            v
        };
        let mut by_name: Vec<_> = s.by_name.iter().collect();
        by_name.sort();
        let mut by_kind: Vec<_> = s.by_kind.iter().collect();
        by_kind.sort();
        let mut by_tag: Vec<_> = s.by_tag.iter().collect();
        by_tag.sort();
        let mut children: Vec<_> = s.children.iter().collect();
        children.sort();
        let mut parent: Vec<_> = s.parent.iter().collect();
        parent.sort();
        let mut ents: Vec<_> = s.ents.iter().collect();
        ents.sort_by_key(|(k, _)| **k);
        let mut tr: Vec<_> = s.transforms.keys().collect();
        tr.sort();
        format!(
            "order {:?}\nnames {:?}\nby_name {by_name:?}\nby_kind {by_kind:?}\nby_tag {by_tag:?}\n\
             children {children:?}\nparent {parent:?}\nents {ents:?}\ntransforms {tr:?}",
            s.order,
            sorted(&s.names)
        )
    }
    let mut world = World::default();
    let root = node(&mut world, 0, None);
    let mut live = vec![root];
    for i in 1..300 {
        let p = (i % 6 == 0).then_some(root);
        live.push(node(&mut world, i, p));
    }
    let host = ScriptHost::new();
    host.sync_scene_for_test(&world);
    let mut seed = 0x2545_f491u32;
    let mut rnd = |n: u32| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed % n
    };
    let mut next = 300;
    for round in 0..60 {
        for _ in 0..rnd(4) {
            if live.len() > 2 {
                let k = 1 + rnd(live.len() as u32 - 1) as usize;
                world.despawn(live.swap_remove(k));
            }
        }
        for _ in 0..rnd(4) {
            let p = (rnd(3) == 0).then(|| live[rnd(live.len() as u32) as usize]);
            live.push(node(&mut world, next, p));
            next += 1;
        }
        let before = crate::host::FULL_SYNCS.with(|c| c.get());
        host.sync_scene_for_test(&world);
        assert_eq!(
            crate::host::FULL_SYNCS.with(|c| c.get()),
            before,
            "round {round}: a spawn/despawn rebuilt the mirror ({:?})",
            host.profile().borrow().mirror_cause()
        );
        let fresh = ScriptHost::new();
        fresh.sync_scene_for_test(&world);
        assert_eq!(snapshot(&host), snapshot(&fresh), "round {round}: the incremental mirror drifted from a rebuild");
    }
}
