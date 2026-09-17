use super::*;

/// `params` is two-way: a script's `params.x = ...` write persists across
/// frames (the next seed reads it back) and lands in the node's stored
/// ScriptInst — the Inspector shows it live. Undeclared keys stay
/// frame-local (they must not silently grow the Inspector).
#[test]
fn param_writes_persist_and_reach_the_stored_params() {
    let dir = std::env::temp_dir().join("floptle_script_test_param_write");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "zoom",
        "defaults = { d = 6 }\nfunction update(node, dt)\n  params.d = params.d - 1\n  params.ghost = 42\nend\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "zoom".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.016, 0.0);
    host.run(&mut world, &dir, 0.016, 0.016);
    host.run(&mut world, &dir, 0.016, 0.032);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let scripts = world.get::<Scripts>(e).unwrap();
    let stored = &scripts.0[0].params;
    let d = stored.iter().find(|(k, _)| k == "d").map(|(_, v)| *v);
    assert_eq!(d, Some(3.0), "the write must persist and decrement each frame: {stored:?}");
    assert!(
        !stored.iter().any(|(k, _)| k == "ghost"),
        "undeclared keys stay frame-local: {stored:?}"
    );
}

/// string params: a `name = "text"` default seeds an Inspector-editable
/// text tunable; stored overrides win over the default, script writes are
/// two-way (persist + reach the stored strs), and undeclared string keys
/// stay frame-local — the numeric rules, for text.
#[test]
fn string_params_seed_override_and_write_two_way() {
    let dir = std::env::temp_dir().join("floptle_script_test_str_params");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "portal",
        "defaults = { scene = \"hub\", label = \"door\" }\n\
         seen = \"\"\n\
         function update(node, dt)\n\
           seen = params.scene .. \"/\" .. params.label\n\
           params.label = \"door2\"\n\
           params.ghost = \"nope\"\n\
         end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "portal".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            // The Inspector override: this portal goes to the arena.
            strs: vec![("scene".into(), "arena".into())],
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.016, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    // The script read the override (not the default) + the default label.
    let env_seen: String = host
        .instance_env(e.index(), "portal")
        .and_then(|env| env.get::<String>("seen").ok())
        .unwrap_or_default();
    assert_eq!(env_seen, "arena/door");
    // The label write persisted to the stored strs; ghost did not.
    let scripts = world.get::<Scripts>(e).unwrap();
    let strs = &scripts.0[0].strs;
    assert_eq!(
        strs.iter().find(|(k, _)| k == "label").map(|(_, v)| v.as_str()),
        Some("door2"),
        "string writes are two-way: {strs:?}"
    );
    assert!(!strs.iter().any(|(k, _)| k == "ghost"), "undeclared stays frame-local");
    // Next frame seeds the persisted write back.
    host.run(&mut world, &dir, 0.016, 0.016);
    let env_seen: String = host
        .instance_env(e.index(), "portal")
        .and_then(|env| env.get::<String>("seen").ok())
        .unwrap_or_default();
    assert_eq!(env_seen, "arena/door2");
}

#[test]
fn params_seeded_from_defaults_without_overrides() {
    // A script with `defaults` but no per-instance overrides must still see params.X
    // (the bug: params was empty, so params.speed read nil).
    let dir = std::env::temp_dir().join("floptle_script_test_params_default");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "spin",
        "defaults = { speed = 90 }\nfunction update(node, dt)\n  node.yaw = node.yaw + math.rad(params.speed) * dt\nend\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst { kind: "spin".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0, 1.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let (yaw, _, _) = world.get::<Transform>(e).unwrap().rotation.to_euler(EulerRot::YXZ);
    assert!((yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-3, "params.speed default not applied; yaw {yaw}");
}

/// **`params` is built when its seed changes, not on every hook call.**
///
/// The table used to be rebuilt from the ECS seed on every pass of every
/// instance — three times a frame per scripted node — and on a scene of
/// sixty scripted nodes that was most of what a node cost. It is now
/// fingerprinted. The guard is a count, watched: force the rebuild back on
/// and this reads twenty instead of one.
#[test]
fn params_are_rebuilt_only_when_the_seed_changes() {
    let dir = std::env::temp_dir().join(format!("floptle_seedfp_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "tunable",
        "defaults = { speed = 1 }\n\
         function update(node, dt) node.y = params.speed end\n\
         function fixedUpdate(node, dt) end\n",
    );
    let (mut world, e) = world_with_script("tunable");
    let mut host = ScriptHost::new();
    crate::host::PARAMS_REBUILDS.with(|c| c.set(0));
    for i in 0..10 {
        let t = i as f32 / 60.0;
        host.run(&mut world, &dir, 1.0 / 60.0, t);
        host.run_fixed(&mut world, 1.0 / 60.0, t);
    }
    assert_eq!(
        crate::host::PARAMS_REBUILDS.with(|c| c.get()),
        1,
        "an unchanged seed must build the table once, not once per hook call"
    );
    assert_eq!(world.get::<Transform>(e).unwrap().translation.y, 1.0, "the default reached the script");

    // The editor changes the seed: the next pass must see it — and cost
    // exactly one more build.
    world.get_mut::<Scripts>(e).unwrap().0[0].params.push(("speed".into(), 2.0));
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0);
    assert_eq!(crate::host::PARAMS_REBUILDS.with(|c| c.get()), 2, "a changed seed rebuilds once");
    assert_eq!(world.get::<Transform>(e).unwrap().translation.y, 2.0, "the new seed reached the script");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **The source decides whether `params` is scanned for writes.** A
/// script that never assigns into `params` is never scanned; one that does
/// is scanned after every hook, and its write still lands in the ECS.
#[test]
fn only_a_script_that_writes_params_is_scanned_for_writes() {
    let dir = std::env::temp_dir().join(format!("floptle_pscan_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "reader", "defaults = { speed = 1 }\nfunction update(node, dt) node.y = params.speed end\n");
    write_script(&dir, "writer", "defaults = { speed = 1 }\nfunction update(node, dt) params.speed = params.speed + 1 end\n");

    let (mut world, e) = world_with_script("reader");
    let mut host = ScriptHost::new();
    crate::host::PARAMS_SCANS.with(|c| c.set(0));
    for i in 0..5 {
        host.run(&mut world, &dir, 1.0 / 60.0, i as f32 / 60.0);
    }
    assert_eq!(crate::host::PARAMS_SCANS.with(|c| c.get()), 0, "a reader was scanned");
    assert_eq!(world.get::<Transform>(e).unwrap().translation.y, 1.0);

    let (mut world, e) = world_with_script("writer");
    let mut host = ScriptHost::new();
    crate::host::PARAMS_SCANS.with(|c| c.set(0));
    for i in 0..3 {
        host.run(&mut world, &dir, 1.0 / 60.0, i as f32 / 60.0);
    }
    assert!(crate::host::PARAMS_SCANS.with(|c| c.get()) >= 3, "a writer must be scanned each hook");
    let seeded = &world.get::<Scripts>(e).unwrap().0[0].params;
    assert!(
        seeded.iter().any(|(k, v)| k == "speed" && *v > 1.0),
        "the write never reached the ECS: {seeded:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The textual test errs toward "writes": every way a script could reach
/// `params` without an obvious assignment counts, so a write can never be
/// missed; only the plainly read-only shapes are exempt.
#[test]
fn the_params_write_test_is_conservative() {
    use crate::source_writes_params as w;
    assert!(w("params.speed = 2"));
    assert!(w("params[\"speed\"] = 2"));
    assert!(w("params[k] = v"));
    assert!(w("  params.x  =  1"));
    assert!(w("local p = params\np.x = 1"), "an alias is a possible write");
    assert!(w("tune(params)"), "handing it to a function is a possible write");
    assert!(w("t = { params }"), "storing it is a possible write");
    assert!(!w("node.y = params.speed"));
    assert!(!w("if params.speed == 2 then end"));
    assert!(!w("local s = params.speed * 2"));
    assert!(!w("print(params.name)"), "a field read passed to a call is a read");
    assert!(!w("myparams.x = 1"), "a longer identifier is not params");
}

/// A scene param the script no longer declares is stored and never read —
/// and from the outside that is indistinguishable from a script whose
/// numbers do nothing. One line, once per session.
#[test]
fn a_scene_param_the_script_does_not_declare_says_so_once() {
    let dir = std::env::temp_dir().join(format!("floptle_0068_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "tool", "defaults = { reach = 4.0 }\nfunction update(node, dt)\nend\n");
    let mut world = World::default();
    // Three instances of the same script, all carrying the stale param —
    // eighteen `sas_button`s must not be eighteen identical lines.
    for i in 0..3 {
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, floptle_core::Name(format!("Belt{i}")));
        world.insert(e, Scripts(vec![floptle_core::ScriptInst {
            kind: "tool".into(),
            enabled: true,
            params: vec![("reach".into(), 6.0), ("laser_range".into(), 26.0)],
            refs: Vec::new(),
            strs: Vec::new(),
        }]));
    }
    let mut host = ScriptHost::new();
    host.set_scene_name("system");
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    let warns: Vec<String> = host
        .drain_logs()
        .into_iter()
        .filter(|l| matches!(l.level, LogLevel::Warn))
        .map(|l| l.msg)
        .collect();
    assert_eq!(warns.len(), 1, "once per (script, param), not per instance: {warns:?}");
    let w = &warns[0];
    assert!(w.contains("laser_range"), "it names the param: {w}");
    assert!(w.contains("system"), "…the scene: {w}");
    assert!(w.contains("Belt"), "…the node: {w}");
    assert!(w.contains("tool"), "…and the script: {w}");
    assert!(!w.contains("reach"), "a param the script DOES declare is not reported: {w}");

    // Still silent on later passes — a warning per frame is a warning
    // nobody reads.
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    assert!(
        host.drain_logs().iter().all(|l| !matches!(l.level, LogLevel::Warn)),
        "reported once, not every frame"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
