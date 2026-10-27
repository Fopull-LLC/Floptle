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
        pinned_bools: None,
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
        Scripts(vec![floptle_core::ScriptInst { kind: "drawer".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new(), pinned_bools: None }]),
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
            stack: Vec::new(),
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

/// **A retag or a rename patches the mirror, and scripts see it.** The patch
/// must not swallow the change: `findTagged` and `find` answer with it the
/// next pass, with no rebuild.
#[test]
fn a_retag_or_rename_patches_the_mirror_and_scripts_see_it() {
    let dir = std::env::temp_dir().join(format!("floptle_mirror_cause_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "seer",
        "function update(node, dt)\n  node.x = #findTagged('main')\n  node.y = find('Lens') and 1 or 0\nend\n",
    );
    let (mut world, e, cam) = level_with_camera("seer", 3);
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    crate::host::FULL_SYNCS.with(|c| c.set(0));
    world.insert(cam, floptle_core::Tags(vec!["main".into()]));
    world.insert(cam, floptle_core::Name("Lens".into()));
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(
        crate::host::FULL_SYNCS.with(|c| c.get()),
        0,
        "a retag rebuilt the mirror ({:?})",
        host.profile().borrow().mirror_cause()
    );
    let t = world.get::<Transform>(e).unwrap().translation;
    assert_eq!((t.x, t.y), (1.0, 1.0), "the scripts did not see the new tag and name");
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
        Scripts(vec![floptle_core::ScriptInst { kind: "oscillate".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new(), pinned_bools: None }]),
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
        Scripts(vec![floptle_core::ScriptInst { kind: "layerer".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new(), pinned_bools: None }]),
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
            pinned_bools: None,
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
        Scripts(vec![floptle_core::ScriptInst { kind: "probe".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new(), pinned_bools: None }]),
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
                pinned_bools: None,
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
            pinned_bools: None,
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
            pinned_bools: None,
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

/// **A spawn, a despawn, a rename, a retag or a reparent is not a rebuild,
/// and says the same thing one would.** After each round of random spawns,
/// despawns and structural edits the scripts' copy of the scene is exactly
/// what a fresh full rebuild makes of the same world (the scene order,
/// first-name-wins, the per-kind and per-tag lists, children) and the
/// incremental host never rebuilt.
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
        // The root is the first "Crate" in scene order. Renaming it away
        // hands the name to the next one; renaming it back takes it back.
        if round % 10 == 5 {
            let name = if round % 20 == 5 { "Root" } else { "Crate" };
            world.insert(root, floptle_core::Name(name.into()));
        }
        // Structural edits on nodes the mirror already holds. Parents only
        // ever point at the root, so no edit can make a cycle.
        for _ in 0..rnd(6) {
            let e = live[1 + rnd(live.len() as u32 - 1) as usize];
            match rnd(5) {
                // Any node may be renamed, the root too: it holds the first
                // "Crate", and a rename has to hand that name to the next one.
                0 => {
                    let e = live[rnd(live.len() as u32) as usize];
                    world.insert(e, floptle_core::Name(if rnd(2) == 0 { "Crate".into() } else { format!("r{next}") }))
                }
                1 => world.insert(e, floptle_core::Tags(vec![format!("t{}", rnd(7)), "enemy".into()])),
                2 => {
                    world.remove::<floptle_core::Tags>(e);
                }
                3 => {
                    if rnd(2) == 0 {
                        world.insert(e, floptle_core::Parent(root));
                    } else {
                        world.remove::<floptle_core::Parent>(e);
                    }
                }
                _ => world.insert(e, Scripts(vec![floptle_core::ScriptInst::new(if rnd(2) == 0 { "ai" } else { "door" })])),
            }
            next += 1;
        }
        let before = crate::host::FULL_SYNCS.with(|c| c.get());
        host.sync_scene_for_test(&world);
        assert_eq!(
            crate::host::FULL_SYNCS.with(|c| c.get()),
            before,
            "round {round}: a spawn, despawn or structural edit rebuilt the mirror ({:?})",
            host.profile().borrow().mirror_cause()
        );
        let fresh = ScriptHost::new();
        fresh.sync_scene_for_test(&world);
        assert_eq!(snapshot(&host), snapshot(&fresh), "round {round}: the incremental mirror drifted from a rebuild");
    }
}

/// **Streaming nodes in costs the nodes, not the scene.** Thirty plants a
/// frame join a 2,000-node level under a "Flora" parent whose tags are
/// rewritten each time, the way a game streams flora as the player walks.
/// That used to rebuild the scripts' whole copy of the scene for every batch.
/// Now nothing rebuilds, only the new and touched nodes are read, and a frame
/// that streams costs little more than one that does not (every pass copies
/// each node's transform either way). Best of twenty frames, so a busy
/// machine slows a reading and never speeds one up.
#[test]
fn streaming_nodes_into_a_big_scene_patches_the_mirror() {
    fn stream(base: usize, per_frame: u32) -> (u64, u32, f64) {
        let mut world = World::default();
        for i in 0..base {
            let e = world.spawn();
            world.insert(e, Transform::from_translation(glam::DVec3::new(i as f64, 0.0, 0.0)));
            world.insert(e, floptle_core::Name(format!("Rock {i}")));
            if i % 10 == 0 {
                world.insert(e, floptle_core::Tags(vec!["rock".into()]));
            }
        }
        let flora = world.spawn();
        world.insert(flora, Transform::IDENTITY);
        world.insert(flora, floptle_core::Name("Flora".into()));
        let host = ScriptHost::new();
        host.profile().borrow_mut().enable(true);
        host.sync_scene_for_test(&world);
        crate::host::FULL_SYNCS.with(|c| c.set(0));
        let (mut best, mut refreshed) = (f64::MAX, 0);
        for f in 0..20u32 {
            for k in 0..per_frame {
                let e = world.spawn();
                world.insert(e, Transform::IDENTITY);
                world.insert(e, floptle_core::Name(format!("Plant {f}.{k}")));
                world.insert(e, floptle_core::Tags(vec!["plant".into()]));
                world.insert(e, floptle_core::Parent(flora));
            }
            if per_frame > 0 {
                world.insert(flora, floptle_core::Tags(vec![format!("wave{f}")]));
            }
            // Three passes a frame, as play runs them; only the first sees
            // the batch.
            let t = std::time::Instant::now();
            for _ in 0..3 {
                host.sync_scene_for_test(&world);
            }
            best = best.min(t.elapsed().as_secs_f64() * 1000.0);
            host.profile().borrow_mut().end_frame();
            refreshed = refreshed.max(host.profile().borrow().mirror_work().refreshed);
        }
        (crate::host::FULL_SYNCS.with(|c| c.get()), refreshed, best)
    }
    // Alternated, best of three each: the suite runs this beside hundreds of
    // other tests, and load landing on one side must not decide the ratio.
    let (mut rebuilds, mut refreshed) = (0, 0);
    let (mut streaming_ms, mut still_ms) = (f64::MAX, f64::MAX);
    for _ in 0..3 {
        let (r, n, ms) = stream(2000, 30);
        (rebuilds, refreshed, streaming_ms) = (rebuilds.max(r), refreshed.max(n), streaming_ms.min(ms));
        still_ms = still_ms.min(stream(2000, 0).2);
    }
    floptle_say::say_err!("mirror per frame in 2,000 nodes: streaming 30 {streaming_ms:.4} ms, streaming none {still_ms:.4} ms");
    assert_eq!(rebuilds, 0, "streaming nodes in rebuilt the mirror");
    assert!(refreshed <= 31, "a frame read {refreshed} nodes for 30 new and one retagged");
    // The counts above are the guard on a rebuild; this bounds what is left.
    // It reads 1.7x here and 2.3x on a CI runner's debug build, since passes
    // with nothing new became cheaper, so it sits well above both.
    assert!(
        streaming_ms < still_ms * 4.0,
        "streaming 30 nodes a frame cost {:.1}x a frame with none",
        streaming_ms / still_ms
    );
}

/// **Streaming never rebuilds, however long it goes on.** Thirty plants join
/// and thirty leave a 2,000-node level every frame for 400 frames — long
/// enough for the world's change log to fill several times over. A full log
/// used to clear itself, which sent the mirror into a full rebuild about every
/// 330 frames of steady streaming: the "every five seconds" a walk through a
/// game's flora showed.
#[test]
fn streaming_for_minutes_never_rebuilds_the_mirror() {
    let mut world = World::default();
    for i in 0..2000 {
        let e = world.spawn();
        world.insert(e, Transform::from_translation(glam::DVec3::new(i as f64, 0.0, 0.0)));
        world.insert(e, floptle_core::Name(format!("Rock {i}")));
    }
    let flora = world.spawn();
    world.insert(flora, Transform::IDENTITY);
    world.insert(flora, floptle_core::Name("Flora".into()));
    let host = ScriptHost::new();
    host.profile().borrow_mut().enable(true);
    host.sync_scene_for_test(&world);
    crate::host::FULL_SYNCS.with(|c| c.set(0));
    let mut live: std::collections::VecDeque<floptle_core::Entity> = Default::default();
    for f in 0..400u32 {
        for k in 0..30 {
            let e = world.spawn();
            world.insert(e, Transform::IDENTITY);
            world.insert(e, floptle_core::Name(format!("Plant {f}.{k}")));
            world.insert(e, floptle_core::Tags(vec!["plant".into()]));
            world.insert(e, floptle_core::Parent(flora));
            live.push_back(e);
        }
        while live.len() > 300 {
            world.despawn(live.pop_front().unwrap());
        }
        world.insert(flora, floptle_core::Tags(vec![format!("wave{f}")]));
        for _ in 0..3 {
            host.sync_scene_for_test(&world);
        }
        host.profile().borrow_mut().end_frame();
        assert_eq!(
            crate::host::FULL_SYNCS.with(|c| c.get()),
            0,
            "frame {f}: streaming rebuilt the mirror ({:?})",
            host.profile().borrow().mirror_cause()
        );
    }
}

/// **A component a `createNode` callback adds is readable afterwards.** The
/// drain mirrors the new node before its callback runs, so the next sync finds
/// the spawn and the callback's write in the same batch, on a node the mirror
/// already holds. Skipping every change on a spawned node dropped the write:
/// `setCamera` in the callback, then `getComponent("Camera")` answered nil.
#[test]
fn a_component_added_by_a_create_callback_reaches_the_mirror() {
    let mut world = World::default();
    let root = world.spawn();
    world.insert(root, Transform::IDENTITY);
    let host = ScriptHost::new();
    host.sync_scene_for_test(&world);
    // The editor's drain: spawn, mirror it for the callback, run the callback.
    let cam = world.spawn();
    world.insert(cam, Transform::IDENTITY);
    world.insert(cam, floptle_core::Name("ZZCam".into()));
    host.sync_new_entities(&world, &[cam]);
    world.insert(
        cam,
        Matter::Camera {
            fov_y: 1.2,
            active: false,
            target: String::new(),
            cull_mask: u32::MAX,
            target_w: Matter::TARGET_W,
            target_h: Matter::TARGET_H,
            target_hz: 0.0,
            ortho: false,
            ortho_height: Matter::ORTHO_HEIGHT,
            stack: Vec::new(),
        },
    );
    host.sync_scene_for_test(&world);
    let s = host.scene.borrow();
    let fov = s.components.get(&cam.index()).and_then(|c| c.get("Camera")).and_then(|c| c.get("fovY"));
    assert_eq!(fov.map(|v| (v * 1e4).round() / 1e4), Some(1.2), "the callback's camera never reached the mirror");
}

/// A node with a script that made a helper somewhere else, and says goodbye.
fn tidy_world(dir: &Path, enabled: bool) -> (World, Entity) {
    write_script(
        dir,
        "tidy",
        "function start(node) helperName = 'Bar' end\n\
         function update(node, dt) end\n\
         function onDestroy(node)\n\
           log('bye ' .. tostring(node.name))\n\
           local h = find(helperName)\n\
           if h then destroy(h) end\n\
         end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, floptle_core::Name("Pulley".into()));
    world.insert(e, floptle_core::Matter::Empty);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "tidy".into(),
            enabled,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
            pinned_bools: None,
        }]),
    );
    let bar = world.spawn();
    world.insert(bar, Transform::IDENTITY);
    world.insert(bar, floptle_core::Name("Bar".into()));
    world.insert(bar, floptle_core::Matter::Empty);
    (world, e)
}

fn byes(host: &mut ScriptHost) -> Vec<String> {
    host.drain_logs().into_iter().filter(|l| l.msg.starts_with("bye")).map(|l| l.msg).collect()
}

/// `onDestroy` runs while the node is still there, can reach what the script
/// made elsewhere, and runs once however many ways the node then goes.
#[test]
fn on_destroy_runs_once_while_the_node_still_exists() {
    let dir = std::env::temp_dir().join("floptle_script_test_on_destroy");
    let _ = std::fs::create_dir_all(&dir);
    let (mut world, e) = tidy_world(&dir, true);
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.016, 0.0);
    host.drain_logs();
    let _ = host.take_destroy_requests();

    host.call_on_destroy_nodes(&mut world, &[e.index()]);
    assert_eq!(byes(&mut host), vec!["bye Pulley".to_string()], "the node's name is still readable");
    let bar = world.query::<floptle_core::Name>().find(|(_, n)| n.0 == "Bar").map(|(b, _)| b.index()).unwrap();
    assert_eq!(host.take_destroy_requests(), vec![bar], "the helper it made is tidied up");

    host.call_on_destroy_nodes(&mut world, &[e.index()]);
    world.despawn(e);
    host.run(&mut world, &dir, 0.016, 0.016);
    assert!(byes(&mut host).is_empty(), "told twice");
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
}

/// A node that goes without anyone calling `onDestroy` first is still told,
/// by the pass that notices its script has nothing to run on.
#[test]
fn a_node_removed_any_other_way_is_still_told() {
    let dir = std::env::temp_dir().join("floptle_script_test_on_destroy_sweep");
    let _ = std::fs::create_dir_all(&dir);
    let (mut world, e) = tidy_world(&dir, true);
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.016, 0.0);
    host.drain_logs();
    world.despawn(e);
    host.run(&mut world, &dir, 0.016, 0.016);
    // The node is gone by now, so its fields read nil; what the script kept
    // is still there, which is what tidying up needs.
    assert_eq!(byes(&mut host), vec!["bye nil".to_string()]);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    host.run(&mut world, &dir, 0.016, 0.032);
    assert!(byes(&mut host).is_empty(), "told again on the next frame");
}

/// A scene switch tells every script that is not coming along, and none that
/// is.
#[test]
fn a_scene_switch_tells_everything_but_what_it_keeps() {
    let dir = std::env::temp_dir().join("floptle_script_test_on_destroy_switch");
    let _ = std::fs::create_dir_all(&dir);
    let (mut world, e) = tidy_world(&dir, true);
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.016, 0.0);
    host.drain_logs();
    let keep: std::collections::HashSet<u32> = [e.index()].into();
    host.call_on_destroy_all_except(&mut world, &keep);
    assert!(byes(&mut host).is_empty(), "a persistent node was told it was going");
    host.call_on_destroy_all_except(&mut world, &Default::default());
    assert_eq!(byes(&mut host).len(), 1);
}

/// Switching a script off is not destroying it.
#[test]
fn a_switched_off_script_is_not_told_it_was_destroyed() {
    let dir = std::env::temp_dir().join("floptle_script_test_on_destroy_off");
    let _ = std::fs::create_dir_all(&dir);
    let (mut world, e) = tidy_world(&dir, true);
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.016, 0.0);
    host.drain_logs();
    world.get_mut::<Scripts>(e).unwrap().0[0].enabled = false;
    host.run(&mut world, &dir, 0.016, 0.016);
    host.run(&mut world, &dir, 0.016, 0.032);
    assert!(byes(&mut host).is_empty(), "switching a script off ran onDestroy");
}

/// A new node under a destroyed node's entity index has its own `onDestroy`.
#[test]
fn a_reused_entity_index_gets_its_own_goodbye() {
    let dir = std::env::temp_dir().join("floptle_script_test_on_destroy_reuse");
    let _ = std::fs::create_dir_all(&dir);
    let (mut world, e) = tidy_world(&dir, true);
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.016, 0.0);
    host.call_on_destroy_nodes(&mut world, &[e.index()]);
    world.despawn(e);
    let e2 = world.spawn();
    assert_eq!(e2.index(), e.index(), "the fixture needs the index reused");
    world.insert(e2, Transform::IDENTITY);
    world.insert(e2, floptle_core::Name("Pulley2".into()));
    world.insert(e2, floptle_core::Matter::Empty);
    world.insert(
        e2,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "tidy".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
            pinned_bools: None,
        }]),
    );
    host.run(&mut world, &dir, 0.016, 0.016);
    host.drain_logs();
    host.call_on_destroy_nodes(&mut world, &[e2.index()]);
    assert_eq!(byes(&mut host), vec!["bye Pulley2".to_string()]);
}

/// **A script's scatter field goes with the script.** A source outlives the
/// call that made it, and nothing knew whose it was: a script that made its
/// field in `start` and was hot-reloaded while it was tuned drew one more copy
/// of the field per save, and a destroyed node left its field standing.
#[test]
fn a_reloaded_or_destroyed_script_takes_its_scatter_field_with_it() {
    let dir = std::env::temp_dir().join(format!("floptle_script_test_scatter_owner_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let body = |n: u32| {
        format!("-- version {n}\nfunction start()\n  scatter.create{{ asset = \"rock.glb\", perChunk = 2 }}\nend\n")
    };
    write_script(&dir, "field", &body(0));
    // A second script's field must survive the first one's reload.
    write_script(&dir, "other", "scatter.create{ asset = \"tree.glb\" }\n");

    let mut world = World::default();
    let spawn = |world: &mut World, kind: &str| {
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Scripts(vec![floptle_core::ScriptInst {
            kind: kind.into(),
            enabled: true,
            params: Vec::new(),
            refs: Vec::new(),
            strs: Vec::new(),
            pinned_bools: None,
        }]));
        e
    };
    let e = spawn(&mut world, "field");
    spawn(&mut world, "other");
    let mut host = ScriptHost::new();
    host.set_playing(true);
    let assets = |host: &ScriptHost| {
        let mut v: Vec<String> = host.scatter_sources().iter().map(|s| s.bands[0].asset.clone()).collect();
        v.sort();
        v
    };
    host.run(&mut world, &dir, 0.016, 0.016);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    assert_eq!(assets(&host), ["rock.glb", "tree.glb"]);

    for n in 1..=2 {
        write_script(&dir, "field", &body(n));
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(10 * n as u64);
        std::fs::File::options().write(true).open(dir.join("field.lua")).unwrap().set_modified(later).unwrap();
        host.run(&mut world, &dir, 0.016, 0.016);
        host.run(&mut world, &dir, 0.016, 0.016);
        assert!(host.errors().is_empty(), "{:?}", host.errors());
        assert_eq!(assets(&host), ["rock.glb", "tree.glb"], "after reload {n}");
    }

    world.despawn(e);
    host.run(&mut world, &dir, 0.016, 0.016);
    assert_eq!(assets(&host), ["tree.glb"], "a destroyed node's field is still standing");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `terrain.planetLakes` answers the lakes the planet's basins hold, without
/// generating it: two craters on a smooth world with no sea give a lake in the
/// shallower one, on the surface of the planet. A misspelt option is refused.
#[test]
fn planet_lakes_reports_the_lakes_a_planet_holds() {
    let dir = std::env::temp_dir().join(format!("floptle_script_test_planet_lakes_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "lakes",
        "local opts = { seed = 8, radius = 150, voxel = 1.5, relief = 1, caveDepth = 0, craters = 2, craterMin = 0.15, craterMax = 0.3 }\n\
         local lakes = terrain.planetLakes(opts, { cell = 3, minDepth = 0.5 })\n\
         local big = lakes[1]\n\
         log(string.format(\"%d %.1f %.1f %.1f\", #lakes, big.center:length() - 150 - big.level, big.depth, big.area))\n\
         local ok, err = pcall(terrain.planetLakes, opts, { mindepth = 1 })\n\
         log(tostring(ok) .. \" \" .. tostring(err))\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, Scripts(vec![floptle_core::ScriptInst {
        kind: "lakes".into(),
        enabled: true,
        params: Vec::new(),
        refs: Vec::new(),
        strs: Vec::new(),
        pinned_bools: None,
    }]));
    let mut host = ScriptHost::new();
    host.set_playing(true);
    host.run(&mut world, &dir, 0.016, 0.016);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    let said: Vec<String> = host.drain_logs().into_iter().map(|l| l.msg).collect();
    let first: Vec<f64> = said[0].split(' ').map(|v| v.parse().unwrap()).collect();
    assert!(first[0] >= 1.0, "no lakes: {said:?}");
    assert!(first[1].abs() < 0.1, "the centre is not on the lake's surface: {said:?}");
    assert!(first[2] > 1.0 && first[3] > 100.0, "{said:?}");
    assert!(said[1].starts_with("false") && said[1].contains("mindepth"), "{said:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **`terrain.pending()` counts the edits that have not landed.** Edits made
/// this frame wait in the script host, then in the engine's queue, which lands
/// a few milliseconds of them a frame; the count is both, so a game knows when
/// its whole batch is in the ground.
#[test]
fn terrain_pending_counts_the_edits_still_to_land() {
    let dir = std::env::temp_dir().join(format!("floptle_script_test_terrain_pending_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "grade",
        "function start()\n\
           for i = 1, 3 do terrain.dig(i, 0, 0, 1) end\n\
           log(tostring(terrain.pending()))\n\
         end\n\
         function update() log(tostring(terrain.pending())) end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, Scripts(vec![floptle_core::ScriptInst {
        kind: "grade".into(),
        enabled: true,
        params: Vec::new(),
        refs: Vec::new(),
        strs: Vec::new(),
        pinned_bools: None,
    }]));
    let mut host = ScriptHost::new();
    host.set_playing(true);
    host.run(&mut world, &dir, 0.016, 0.016);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    // The engine takes them, lands one, and says two are still to come.
    let taken = host.take_terrain_ops();
    assert_eq!(taken.len(), 3);
    host.set_terrain_edits_queued(2);
    host.run(&mut world, &dir, 0.016, 0.016);
    let said: Vec<String> = host.drain_logs().into_iter().map(|l| l.msg).collect();
    assert_eq!(said.first().map(String::as_str), Some("3"), "{said:?}");
    assert_eq!(said.last().map(String::as_str), Some("2"), "{said:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A planet question asked with a callback costs the script thread almost
/// nothing.** `terrain.planetLakes` on an 800 m world took 7 s on the script
/// thread, past the watchdog. Given a callback, the call returns at once, the
/// work runs on a thread of its own, and the callback gets the same answer the
/// plain call gives in a later frame pass.
#[test]
fn planet_questions_with_a_callback_answer_off_the_script_thread() {
    let dir = std::env::temp_dir().join(format!("floptle_script_test_planet_async_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "ask",
        "local opts = { seed = 8, radius = 150, voxel = 1.5, relief = 4, caveDepth = 0, craters = 2, craterMin = 0.15, craterMax = 0.3,\n\
                         mountains = { height = 12 }, rivers = { count = 3, seaLevel = -2 } }\n\
         function start()\n\
           local t0 = os.clock()\n\
           local lakes = terrain.planetLakes(opts, { minDepth = 0.5 })\n\
           local rivers = terrain.planetRivers(opts)\n\
           local t1 = os.clock()\n\
           local none = terrain.planetLakes(opts, { minDepth = 0.5 }, function(l) gotLakes = l end)\n\
           terrain.planetRivers(opts, function(r) gotRivers = r end)\n\
           local t2 = os.clock()\n\
           log(string.format(\"%s %d %d %.6f %.6f %s\", tostring(none), #lakes, #rivers, t1 - t0, t2 - t1, tostring(gotLakes)))\n\
           want = { #lakes, lakes[1] and lakes[1].level or 0, #rivers }\n\
         end\n\
         function update()\n\
           if gotLakes and gotRivers and not said then\n\
             said = true\n\
             log(string.format(\"%d %d %.4f %.4f %d %d\", #gotLakes, want[1], gotLakes[1] and gotLakes[1].level or 0, want[2], #gotRivers, want[3]))\n\
           end\n\
         end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, Scripts(vec![floptle_core::ScriptInst {
        kind: "ask".into(),
        enabled: true,
        params: Vec::new(),
        refs: Vec::new(),
        strs: Vec::new(),
        pinned_bools: None,
    }]));
    let mut host = ScriptHost::new();
    host.set_playing(true);
    let mut said: Vec<String> = Vec::new();
    for _ in 0..3000 {
        host.run(&mut world, &dir, 0.016, 0.016);
        assert!(host.errors().is_empty(), "{:?}", host.errors());
        said.extend(host.drain_logs().into_iter().map(|l| l.msg));
        if said.len() >= 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(said.len(), 2, "the callbacks never ran: {said:?}");
    let first: Vec<&str> = said[0].split(' ').collect();
    assert_eq!(first[0], "nil", "a call with a callback answers nothing itself: {said:?}");
    assert_eq!(first[5], "nil", "the callback ran inside the call: {said:?}");
    let (sync, ask): (f64, f64) = (first[3].parse().unwrap(), first[4].parse().unwrap());
    assert!(ask < 0.1 * sync, "asking with a callback took {ask} s on the script thread, the plain calls {sync} s");
    let second: Vec<f64> = said[1].split(' ').map(|v| v.parse().unwrap()).collect();
    assert!(second[1] >= 1.0 && second[5] >= 1.0, "nothing to compare: {said:?}");
    assert_eq!((second[0], second[2], second[4]), (second[1], second[3], second[5]), "the callback's answer differs from the plain call's: {said:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **`scatter.near` on a planet's field answers on the planet from the start.**
/// A source's region is in its anchor's frame, and that frame was set only
/// when the field was drawn: on the frame a source was created, and in every
/// frame of a headless `run`, `near` asked about the world origin instead and
/// found nothing beside a player standing on the surface.
#[test]
fn scatter_near_on_a_parented_field_is_right_without_a_draw() {
    let dir = std::env::temp_dir().join(format!("floptle_script_test_scatter_near_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "flora",
        "function start(node)\n\
         \x20 id = scatter.create{ asset = 'rock.glb', parent = 'Planet', center = vec3(0, 0, 0), radius = 100, perChunk = 40 }\n\
         \x20 first = #scatter.near(id, vec3(5000, 100, 0), 12)\n\
         end\n\
         function update(node, dt)\n\
         \x20 local p = find('Planet')\n\
         \x20 later = #scatter.near(id, vec3(p.x, p.y + 100, p.z), 12)\n\
         end\n",
    );
    let mut world = World::default();
    let planet = world.spawn();
    world.insert(planet, Transform::from_translation(glam::DVec3::new(5000.0, 0.0, 0.0)));
    world.insert(planet, floptle_core::Name("Planet".into()));
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, Scripts(vec![floptle_core::ScriptInst::new("flora")]));
    let mut host = ScriptHost::new();
    // The planet moves on between frames, as one on rails does.
    for i in 0..3 {
        host.run(&mut world, &dir, 1.0 / 60.0, i as f32 / 60.0);
        assert!(host.errors().is_empty(), "{:?}", host.errors());
        world.get_mut::<Transform>(planet).unwrap().translation.x += 700.0;
    }
    let env = host.instance_env(e.index(), "flora").unwrap();
    let (first, later): (u32, u32) = (env.get("first").unwrap(), env.get("later").unwrap());
    assert!(first > 0, "the creation frame's near found nothing on the planet");
    assert!(later > 0, "a frame that was never drawn found nothing on the planet");
}

/// **A busy machine does not stop an honest script; an endless loop is still
/// stopped.** The watchdog's clock was the wall: on a machine loaded on every
/// core, a pass doing a few tens of milliseconds of work waited its turn for a
/// core past the budget and the script was stopped for good, mid-build. Here
/// a script does about a third of its budget in work every pass, with every
/// core kept busy several times over, and keeps running; a loop that never
/// ends is stopped with its CPU and wall time in the message.
#[test]
#[cfg(any(unix, windows))]
fn a_loaded_machine_does_not_stop_an_honest_script() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let dir = std::env::temp_dir().join(format!("floptle_script_test_load_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "worker",
        "passes = 0\nfunction update(node, dt)\n  local x = 0\n  for i = 1, N do x = x + i % 7 end\n  \
         passes = passes + 1\nend\n",
    );
    write_script(&dir, "spinner", "function update(node, dt) while true do end end\n");
    let budget = std::time::Duration::from_millis(150);
    let world_with = |kind: &str| {
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Scripts(vec![floptle_core::ScriptInst::new(kind)]));
        (world, e)
    };
    let (mut world, e) = world_with("worker");
    let mut host = ScriptHost::new();
    host.set_script_budget(budget);
    // Calibrate N to about 50 ms of this thread's CPU, unloaded.
    host.lua.globals().set("N", 200_000).unwrap();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    let c0 = crate::budget::thread_cpu().expect("this platform has a thread clock");
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    let per = (crate::budget::thread_cpu().unwrap() - c0).as_secs_f64().max(1e-4);
    let n = (200_000.0 * 0.05 / per) as i64;
    host.lua.globals().set("N", n).unwrap();

    // Every core busy four times over: a fair share of a core is about a
    // quarter, so 50 ms of work takes about 200 ms of wall, past the budget.
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let spinners: Vec<_> = (0..cores * 4)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut x = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
                }
            })
        })
        .collect();
    let start = std::time::Instant::now();
    let mut frames = 0;
    while start.elapsed() < std::time::Duration::from_secs(3) {
        host.run(&mut world, &dir, 1.0 / 60.0, frames as f32 / 60.0);
        frames += 1;
    }
    stop.store(true, Ordering::Relaxed);
    for s in spinners {
        let _ = s.join();
    }
    assert!(host.errors().is_empty(), "a loaded machine stopped the worker: {:?}", host.errors());
    let passes: i64 = host.instance_env(e.index(), "worker").unwrap().get("passes").unwrap();
    assert_eq!(passes, frames + 2, "the worker stopped running");

    // An endless loop is still stopped, and says what it spent.
    let (mut world, _) = world_with("spinner");
    let mut host = ScriptHost::new();
    host.set_script_budget(budget);
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    let e = host.errors().join("\n");
    assert!(crate::budget::Budget::is_timeout(&e), "the endless loop ran on: {e}");
    assert!(e.contains("of CPU") && e.contains("of wall time"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A planet a script asks for without naming a generator is made by the
/// project's.** The host's version reaches `generatePlanet` (which carries it
/// to the engine), and `planetRivers` answers with the rivers of the world
/// that version makes: the old march at version 0, the drainage after. A
/// named version is kept, and one past this engine's is refused.
#[test]
fn a_planet_asked_for_without_a_generator_is_made_by_the_projects() {
    use floptle_field::procgen::GENERATOR_VERSION;
    let dir = std::env::temp_dir().join(format!("floptle_script_test_planet_gen_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "make",
        "local opts = { seed = 31, radius = 60, voxel = 1.5, relief = 5, caveDepth = 0, rivers = { count = 3 } }\n\
         function start()\n\
           terrain.generatePlanet(1, opts)\n\
           terrain.generatePlanet(2, { radius = 40, generator = 1 })\n\
           local ok, err = pcall(terrain.generatePlanet, 3, { generator = 99 })\n\
           log(tostring(err))\n\
           local r = terrain.planetRivers(opts)[1].points\n\
           log(string.format(\"%d %.4f %.4f %.4f\", #r, r[4], r[5], r[6]))\n\
         end\n",
    );
    let run = |generator: Option<u32>| {
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Scripts(vec![floptle_core::ScriptInst {
            kind: "make".into(),
            enabled: true,
            params: Vec::new(),
            refs: Vec::new(),
            strs: Vec::new(),
            pinned_bools: None,
        }]));
        let host = ScriptHost::new();
        if let Some(v) = generator {
            host.set_terrain_generator(v);
        }
        let mut host = host;
        host.set_playing(true);
        host.run(&mut world, &dir, 0.016, 0.016);
        assert!(host.errors().is_empty(), "{:?}", host.errors());
        let said: Vec<String> = host.drain_logs().into_iter().map(|l| l.msg).collect();
        let made: Vec<Option<u32>> = host.take_terrain_generates().into_iter().map(|(_, f)| f.generator).collect();
        (said, made)
    };
    let (old_said, old_made) = run(Some(0));
    assert_eq!(old_made, [Some(0), Some(1)], "the project's version, then the named one");
    assert!(old_said[0].contains("version"), "a version past this engine's was taken: {old_said:?}");
    let (new_said, new_made) = run(None);
    assert_eq!(new_made, [Some(GENERATOR_VERSION), Some(1)], "a new host is not the current generator");
    assert_ne!(old_said[1], new_said[1], "the rivers asked about are not the world's: {old_said:?} {new_said:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **`terrain.impostorAt` hands the engine a body's rule, in order, and
/// refuses one it cannot use.** Radii or pixels, not both; no rule is the
/// default again.
#[test]
fn impostor_rules_reach_the_engine_in_order() {
    use crate::ImpostorRule;
    let dir = std::env::temp_dir().join(format!("floptle_script_test_impostor_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "far",
        "function start()\n\
           terrain.impostorAt(3, { radii = 20 })\n\
           terrain.impostorAt(4, { pixels = 48 })\n\
           terrain.impostorAt(3)\n\
           local a = select(2, pcall(terrain.impostorAt, 5, { radii = 2, pixels = 3 }))\n\
           local b = select(2, pcall(terrain.impostorAt, 5, { pixels = 0 }))\n\
           local c = select(2, pcall(terrain.impostorAt, 5, { radius = 4 }))\n\
           log(tostring(a)) log(tostring(b)) log(tostring(c))\n\
         end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, Scripts(vec![floptle_core::ScriptInst {
        kind: "far".into(),
        enabled: true,
        params: Vec::new(),
        refs: Vec::new(),
        strs: Vec::new(),
        pinned_bools: None,
    }]));
    let mut host = ScriptHost::new();
    host.set_playing(true);
    host.run(&mut world, &dir, 0.016, 0.016);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    assert_eq!(
        host.take_terrain_impostors(),
        [(3, Some(ImpostorRule::Radii(20.0))), (4, Some(ImpostorRule::Pixels(48.0))), (3, None)]
    );
    assert!(host.take_terrain_impostors().is_empty());
    let said: Vec<String> = host.drain_logs().into_iter().map(|l| l.msg).collect();
    assert!(said[0].contains("not both"), "{said:?}");
    assert!(said[1].contains("more than 0"), "{said:?}");
    assert!(said[2].contains("radius"), "an unknown key is not named: {said:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **`assets.warm` sends each path to the side that prepares it, by its
/// extension, and one callback waits for every kind.** An answer for one kind
/// does not erase another kind's answer given a frame earlier.
#[test]
fn a_warm_sends_each_kind_to_its_side_and_waits_for_all_of_them() {
    use crate::PreloadKind::{Material, Model, Shader, Sound, Texture};
    let dir = std::env::temp_dir().join("floptle_script_test_warm");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "warm",
        "calls = 0\n\
         function start(node)\n\
           assets.warm({ \"textures/hull.PNG\", \"shaders/ocean.flsl\", \"materials/steel.ron\", \"models/arm.glb\", \"audio/hit.wav\" }, function(failed)\n\
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
            pinned_bools: None,
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(host.take_preload_requests(Texture), vec!["textures/hull.PNG".to_string()]);
    assert_eq!(host.take_preload_requests(Shader), vec!["shaders/ocean.flsl".to_string()]);
    assert_eq!(host.take_preload_requests(Material), vec!["materials/steel.ron".to_string()]);
    assert_eq!(host.take_preload_requests(Model), vec!["models/arm.glb".to_string()]);
    assert_eq!(host.take_preload_requests(Sound), vec!["audio/hit.wav".to_string()]);
    let calls = |host: &ScriptHost| -> i64 { host.instance_env(e.index(), "warm").unwrap().get("calls").unwrap() };

    // Four of five answered over three frames: still waiting, and the early
    // answers still stand.
    host.set_preload_status(Texture, HashMap::from([("textures/hull.PNG".to_string(), true)]));
    host.set_preload_status(Model, HashMap::from([("models/arm.glb".to_string(), true)]));
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    host.set_preload_status(Sound, HashMap::from([("audio/hit.wav".to_string(), true)]));
    host.set_preload_status(Material, HashMap::from([("materials/steel.ron".to_string(), true)]));
    host.run(&mut world, &dir, 1.0 / 60.0, 2.0 / 60.0);
    assert_eq!(calls(&host), 0, "called back before the shader had answered");

    host.set_preload_status(Shader, HashMap::from([("shaders/ocean.flsl".to_string(), false)]));
    host.run(&mut world, &dir, 1.0 / 60.0, 3.0 / 60.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let env = host.instance_env(e.index(), "warm").unwrap();
    assert_eq!(calls(&host), 1, "every kind had answered");
    assert_eq!(env.get::<i64>("failedCount").unwrap(), 1);
    assert_eq!(env.get::<String>("firstFailed").unwrap(), "shaders/ocean.flsl");
}

/// One frame of the engine's side of the terrain queue: take what scripts
/// queued, land up to `per_frame` of everything waiting, and report the rest.
fn land_terrain_edits(host: &ScriptHost, waiting: &mut Vec<crate::TerrainOp>, per_frame: usize) -> Vec<crate::TerrainOp> {
    waiting.extend(host.take_terrain_ops());
    let n = per_frame.min(waiting.len());
    let landed: Vec<crate::TerrainOp> = waiting.drain(..n).collect();
    host.set_terrain_edits_queued(waiting.len());
    landed
}

fn terrain_host(dir: &Path, name: &str, body: &str) -> (ScriptHost, World) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).unwrap();
    write_script(dir, name, body);
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, Scripts(vec![floptle_core::ScriptInst {
        kind: name.into(),
        enabled: true,
        params: Vec::new(),
        refs: Vec::new(),
        strs: Vec::new(),
        pinned_bools: None,
    }]));
    let host = ScriptHost::new();
    host.set_playing(true);
    (host, world)
}

/// **A big batch sent at `terrain.capacity()` a frame all lands.** Solar's
/// base grade sent 24,192 edits in one frame and about 20,000 were dropped
/// with a single warning. Paced by the room the queue says it has, every edit
/// gets a real receipt (paint and paintTexture included), nothing is
/// refused, and every one reaches the engine.
#[test]
fn a_batch_paced_by_capacity_never_loses_an_edit() {
    let dir = std::env::temp_dir().join(format!("floptle_script_test_terrain_capacity_{}", std::process::id()));
    let (mut host, mut world) = terrain_host(
        &dir,
        "grade",
        "local total, sent, zero = 24192, 0, 0\n\
         function update()\n\
           local n = math.min(terrain.capacity(), total - sent)\n\
           for i = 1, n do\n\
             sent = sent + 1\n\
             local k, id = sent % 3\n\
             if k == 0 then id = terrain.dig(sent, 0, 0, 1)\n\
             elseif k == 1 then id = terrain.paint(sent, 0, 0, 1, 1, 0, 0)\n\
             else id = terrain.paintTexture(sent, 0, 0, 1, 2) end\n\
             if id == 0 then zero = zero + 1 end\n\
           end\n\
           log(string.format('%d %d %d', sent, zero, perf.counts().terrainDropped))\n\
         end\n",
    );
    let mut waiting = Vec::new();
    let mut landed = Vec::new();
    for _ in 0..200 {
        host.run(&mut world, &dir, 0.016, 0.016);
        assert!(host.errors().is_empty(), "{:?}", host.errors());
        // The engine lands about a quarter of the cap a frame: slower than the
        // game sends, so the queue fills and pacing is what keeps it whole.
        landed.extend(land_terrain_edits(&host, &mut waiting, 1000));
        if landed.len() == 24192 {
            break;
        }
    }
    let said: Vec<String> = host.drain_logs().into_iter().map(|l| l.msg).collect();
    assert_eq!(landed.len(), 24192, "{} of 24192 edits landed: {:?}", landed.len(), said.last());
    assert_eq!(said.last().map(String::as_str), Some("24192 0 0"), "{said:?}");
    let ids: std::collections::HashSet<u64> = landed.iter().map(|o| o.id).collect();
    assert_eq!(ids.len(), 24192, "receipts must be distinct");
    assert!(!ids.contains(&0), "a landed edit carried the no-receipt id");
    assert_eq!(host.terrain_edits_dropped(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// **An edit past the cap is refused where a script can see it.** Paint used
/// to answer nothing at all, so a dropped paint was invisible; now every verb
/// answers 0 for a refused edit and each one is counted, not just the first
/// warned about.
#[test]
fn an_edit_past_the_cap_answers_zero_and_is_counted() {
    let dir = std::env::temp_dir().join(format!("floptle_script_test_terrain_cap_{}", std::process::id()));
    let (mut host, mut world) = terrain_host(
        &dir,
        "flood",
        "function start()\n\
           local zero = 0\n\
           for i = 1, 5000 do\n\
             if terrain.paint(i, 0, 0, 1, 1, 1, 1) == 0 then zero = zero + 1 end\n\
           end\n\
           log(string.format('%d %d %d', zero, terrain.capacity(), perf.counts().terrainDropped))\n\
         end\n",
    );
    host.run(&mut world, &dir, 0.016, 0.016);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    let said: Vec<String> = host.drain_logs().into_iter().map(|l| l.msg).collect();
    let refused = 5000 - crate::MAX_PENDING_OPS;
    assert!(said.iter().any(|l| *l == format!("{refused} 0 {refused}")), "{said:?}");
    assert_eq!(host.terrain_edits_dropped(), refused as u64);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A driver node running `name` with `n` children named part1..partN.
fn craft(n: usize, name: &str) -> (World, Entity) {
    let mut world = World::default();
    let root = world.spawn();
    world.insert(root, Transform::IDENTITY);
    world.insert(root, floptle_core::Name("Craft".into()));
    world.insert(root, Scripts(vec![floptle_core::ScriptInst {
        kind: name.into(),
        enabled: true,
        params: Vec::new(),
        refs: Vec::new(),
        strs: Vec::new(),
        pinned_bools: None,
    }]));
    for i in 0..n {
        let e = world.spawn();
        world.insert(e, Transform::from_translation(floptle_core::math::DVec3::new(i as f64, 2.0, 3.0)));
        world.insert(e, floptle_core::Name(format!("part{}", i + 1)));
        world.insert(e, floptle_core::Parent(root));
    }
    (world, root)
}

/// **Reading a vec3's component is reading a table field.** As a userdata,
/// each `.x` was a call into the host, and a loop reading three a part over
/// a thousand parts went quadratic in calls. An `exact` vec3 holds its
/// components as its own fields, so the VM reads them without leaving Lua:
/// `rawget` finds them, and a read costs what any table read costs (a
/// ratio, not a duration: it was 15 to 30 times one).
#[test]
fn a_vec3_component_is_a_field_the_vm_reads() {
    let dir = std::env::temp_dir().join(format!("floptle_script_test_vec3_field_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "reader",
        "function update(node)\n\
           local v, t = vec3(1, 2, 3), { x = 1, y = 2, z = 3 }\n\
           local p = node:children()[1].pos\n\
           log(string.format('RAW %s %s %s', type(v), tostring(rawget(v, 'x')), tostring(rawget(v, 'z'))))\n\
           log(string.format('POS %s %s %s', type(p), tostring(type(p) == 'table' and rawget(p, 'y')), tostring(type(p) == 'table' and rawget(p, 'z'))))\n\
           local best = math.huge\n\
           for k = 1, 5 do\n\
             local s, t0 = 0, os.clock()\n\
             for i = 1, 100000 do s = s + p.x + p.y + p.z end\n\
             local tv = os.clock() - t0\n\
             s, t0 = 0, os.clock()\n\
             for i = 1, 100000 do s = s + t.x + t.y + t.z end\n\
             local tt = os.clock() - t0\n\
             best = math.min(best, tv / math.max(tt, 1e-9))\n\
           end\n\
           log(string.format('RATIO %.2f', best))\n\
         end\n",
    );
    let (mut world, _) = craft(1, "reader");
    let mut host = ScriptHost::new();
    host.set_playing(true);
    host.run(&mut world, &dir, 0.016, 0.016);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    let said: Vec<String> = host.drain_logs().into_iter().map(|l| l.msg).collect();
    assert!(said.iter().any(|l| l == "RAW table 1 3"), "vec3()'s components are not its own fields: {said:?}");
    assert!(said.iter().any(|l| l == "POS table 2 3"), "an engine-made vec3's components are not its own fields: {said:?}");
    let ratio: f64 = said.iter().find_map(|l| l.strip_prefix("RATIO ")).and_then(|r| r.parse().ok()).unwrap();
    assert!(ratio < 3.0, "a vec3 component read costs {ratio:.1} times a table field read: {said:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A child is found by id in one lookup, and a kept child list knows when
/// it is stale.** `node:children()` builds a handle per child on every call,
/// so a lookup inside a per-part loop was quadratic. `node:child(id)` answers
/// the direct child with that id however many there are (counted: a thousand
/// children cost what ten do, within noise), a number that is no child's id
/// is still a name, and `node:childrenGeneration()` stays put across frames
/// until the list changes, then moves.
#[test]
fn a_child_by_id_is_one_lookup_and_the_children_generation_marks_changes() {
    let dir = std::env::temp_dir().join(format!("floptle_script_test_child_id_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "kids",
        "function update(node)\n\
           local kids = node:children()\n\
           local found = 0\n\
           for i = 1, #kids do if node:child(kids[i].id) and node:child(kids[i].id).id == kids[i].id then found = found + 1 end end\n\
           local t0 = os.clock()\n\
           for r = 1, 20 do for i = 1, 10 do node:child(kids[i].id) end end\n\
           local per = (os.clock() - t0) / 200\n\
           log(string.format('KIDS %d %d %s %s %d %.9f', #kids, found, tostring(node:child(kids[1].id + 100000)), tostring(node:child('part2') and node:child('part2').name), node:childrenGeneration(), per))\n\
         end\n",
    );
    let per_lookup = |n: usize| -> (Vec<String>, World, Entity, ScriptHost) {
        let (mut world, root) = craft(n, "kids");
        let mut host = ScriptHost::new();
        host.set_playing(true);
        host.run(&mut world, &dir, 0.016, 0.016);
        assert!(host.errors().is_empty(), "{:?}", host.errors());
        let said: Vec<String> = host.drain_logs().into_iter().map(|l| l.msg).collect();
        (said, world, root, host)
    };
    let field = |l: &str, k: usize| l.split(' ').nth(k).unwrap().to_string();
    let (small, _, _, _) = per_lookup(10);
    let (big, mut world, root, mut host) = per_lookup(1000);
    let line = big.iter().find(|l| l.starts_with("KIDS ")).unwrap().clone();
    assert_eq!(field(&line, 1), "1000");
    assert_eq!(field(&line, 2), "1000", "a child by its id was not found: {line}");
    assert_eq!(field(&line, 3), "nil", "an id that is no child's answered a node: {line}");
    assert_eq!(field(&line, 4), "part2", "a name still finds its child: {line}");
    let t_small: f64 = field(small.iter().find(|l| l.starts_with("KIDS ")).unwrap(), 6).parse().unwrap();
    let t_big: f64 = field(&line, 6).parse().unwrap();
    assert!(t_big < t_small * 4.0 + 2e-6, "a lookup among 1000 children cost {t_big:e} s against {t_small:e} among 10");

    // Unchanged across a frame with nothing moved; moved by a child added.
    let generation = |host: &mut ScriptHost, world: &mut World| -> String {
        host.run(world, &dir, 0.016, 0.016);
        let said: Vec<String> = host.drain_logs().into_iter().map(|l| l.msg).collect();
        field(said.iter().find(|l| l.starts_with("KIDS ")).unwrap(), 5)
    };
    let g0 = field(&line, 5);
    assert_eq!(generation(&mut host, &mut world), g0, "nothing changed and the generation moved");
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, floptle_core::Parent(root));
    let g1 = generation(&mut host, &mut world);
    assert_ne!(g1, g0, "a child was added and the generation did not move");
    // An unrelated node elsewhere leaves it alone; a child going moves it.
    let other = world.spawn();
    world.insert(other, Transform::IDENTITY);
    assert_eq!(generation(&mut host, &mut world), g1, "a node outside the list moved its generation");
    world.despawn(e);
    assert_ne!(generation(&mut host, &mut world), g1, "a child went and the generation did not move");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A burst of spawn callbacks writes each node back once.** The engine
/// runs a spawn's callback, then flushes what it wrote; the set of written
/// nodes was kept until the next pass, so each of a thousand callbacks wrote
/// every node moved so far again: half a million transform writes for a
/// thousand parts, each logged as a change. Counted: a thousand callbacks
/// that each move their own node write a thousand transforms, and the next
/// pass patches the mirror rather than rebuilding it.
#[test]
fn a_burst_of_spawn_callbacks_writes_each_node_back_once() {
    let dir = std::env::temp_dir().join(format!("floptle_script_test_spawn_burst_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "builder",
        "local done = false\n\
         function update(node)\n\
           if done then return end\n\
           done = true\n\
           for i = 1, 1000 do spawn('part', vec3(i, 0, 0), function(p) p.y = 1 end) end\n\
         end\n",
    );
    let (mut world, _) = craft(0, "builder");
    let mut host = ScriptHost::new();
    host.set_playing(true);
    host.run(&mut world, &dir, 0.016, 0.016);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    let reqs = host.take_spawn_requests();
    assert_eq!(reqs.len(), 1000);
    let before = crate::host::FLUSHED_TRANSFORMS.with(|c| c.get());
    for r in reqs {
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        host.call_spawn_callback(&mut world, r.cb.unwrap(), e.index(), &[e]);
    }
    let flushed = crate::host::FLUSHED_TRANSFORMS.with(|c| c.get()) - before;
    assert!(flushed <= 1000, "a thousand callbacks wrote {flushed} transforms back");
    let moved = world.query::<Transform>().filter(|(_, t)| t.translation.y == 1.0).count();
    assert_eq!(moved, 1000, "a callback's write did not land");
    let full = crate::host::FULL_SYNCS.with(|c| c.get());
    host.run(&mut world, &dir, 0.016, 0.016);
    assert_eq!(crate::host::FULL_SYNCS.with(|c| c.get()), full, "the pass after the burst rebuilt the whole mirror");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A spawn's callback can hand the new node's script its setup data.** The
/// spawned node's scripts were loaded on the pass after the spawn, so in the
/// callback their handle read nil and `crew.bind(7)` was lost with a warning:
/// a seated crew member stayed at its spawn point. Now the scripts load before
/// the callback, and `start` still runs on the next pass, after the bind.
#[test]
fn a_spawn_callback_reaches_the_new_node_s_script() {
    let dir = std::env::temp_dir().join(format!("floptle_script_test_spawn_bind_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "spawner",
        "local done = false\n\
         function update(node)\n\
           if done then return end\n\
           done = true\n\
           spawn('crew', vec3(0, 0, 0), function(c) c:getScript('crew').bind(7) end)\n\
         end\n",
    );
    write_script(
        &dir,
        "crew",
        "local seat = nil\n\
         local started = false\n\
         function bind(id) seat = id end\n\
         function start(node) started = true; node.x = seat or -1 end\n",
    );
    let (mut world, _) = craft(0, "spawner");
    let mut host = ScriptHost::new();
    host.set_playing(true);
    host.run(&mut world, &dir, 0.016, 0.016);
    let reqs = host.take_spawn_requests();
    assert_eq!(reqs.len(), 1);
    // The driver spawns the prefab: a node carrying the crew script.
    let crew = world.spawn();
    world.insert(crew, Transform::IDENTITY);
    world.insert(crew, Scripts(vec![floptle_core::ScriptInst {
        kind: "crew".into(),
        enabled: true,
        params: Vec::new(),
        refs: Vec::new(),
        strs: Vec::new(),
        pinned_bools: None,
    }]));
    let r = reqs.into_iter().next().unwrap();
    host.call_spawn_callback(&mut world, r.cb.unwrap(), crew.index(), &[crew]);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    let warned: Vec<_> = host.drain_logs().into_iter().filter(|l| l.msg.contains("not running")).collect();
    assert!(warned.is_empty(), "the callback was told the script is not running: {warned:?}");
    assert_eq!(world.get::<Transform>(crew).unwrap().translation.x, 0.0, "start ran before the next pass");
    host.run(&mut world, &dir, 0.016, 0.016);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    assert_eq!(world.get::<Transform>(crew).unwrap().translation.x, 7.0, "the bind made in the spawn callback was lost");
    let _ = std::fs::remove_dir_all(&dir);
}
