use super::*;

/// A library script — no `start`, no `update`, just functions other scripts
/// call — must have its `params` before anybody calls into it.
///
/// `params` used to be seeded by the tick, so a script that never ticks
/// never got one and the first caller into any of its functions got
/// `attempt to index global 'params' (a nil value)`. Worse than a plain nil:
/// whether a hookless script had been seeded depended on whether something
/// else had ticked it first, which depends on scene order — so the same
/// project worked on one machine and raised on another, and adding an
/// unrelated node could fix it. In the solar game this one error read as
/// four broken features (no inventory, no selling, no HUD count).
#[test]
fn a_hookless_library_script_has_its_params_before_anybody_calls_in() {
    let dir = std::env::temp_dir().join(format!("floptle-libparams-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // No start, no update: this script exists to be called.
    write_script(
        &dir,
        "inventory",
        concat!(
            // `@node` is a reference param: the Inspector wires it to a node
            // and the script reads it as a handle.
            "defaults = { cap = 40, owner = noderef() }\n",
            "function cap()\n",
            "  return params.cap\n",
            "end\n",
            "function ownerName()\n",
            "  return params.owner and params.owner.name or 'nobody'\n",
            "end\n",
        ),
    );
    // …and the caller asks on the very first frame, from `start`.
    write_script(
        &dir,
        "hud",
        concat!(
            "function start(node)\n",
            "  local inv = findScript('inventory')\n",
            "  log('cap=' .. tostring(inv:cap()))\n",
            "  log('owner=' .. tostring(inv:ownerName()))\n",
            "end\n",
        ),
    );

    let mut world = World::default();
    // The library node comes second in scene order on purpose: it is the
    // order that used to decide whether this worked.
    let hud = world.spawn();
    world.insert(hud, Transform::IDENTITY);
    world.insert(hud, floptle_core::Name("Hud".into()));
    world.insert(
        hud,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "hud".into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );
    let bag = world.spawn();
    world.insert(bag, Transform::IDENTITY);
    world.insert(bag, floptle_core::Name("Inventory".into()));
    world.insert(
        bag,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "inventory".into(),
            enabled: true,
            // The Inspector's own value, which is what the caller must read
            // — not the `defaults` line and not nil.
            params: vec![("cap".into(), 55.0)],
            // …wired to the HUD node, by name, as the Inspector wires one.
            refs: vec![("owner".into(), "Hud".into())],
            strs: Vec::new(),
        }]),
    );

    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let logs: Vec<String> = host.drain_logs().into_iter().map(|l| l.msg).collect();
    assert!(
        logs.iter().any(|l| l == "cap=55.0" || l == "cap=55"),
        "the library answered from its Inspector params: {logs:?}"
    );
    // A hookless script never ticks, so the seed is the only chance its
    // reference params ever get to be resolved.
    assert!(
        logs.iter().any(|l| l == "owner=Hud"),
        "a wired reference param has to be there too, or it is nil forever: {logs:?}"
    );
}

/// The editor-action path end-to-end at the script layer: `call_action`
/// runs exactly the named function (never `start`), the construction API
/// (`setCelestial`/`setMaterial`) lands on the world, and `createNode` +
/// `terrain.generatePlanet` sit queued for the editor to drain.
#[test]
fn editor_action_runs_one_function_and_queues_construction() {
    let dir = std::env::temp_dir().join(format!("floptle-action-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "gen",
        r#"
--@editorButton Generate roll
defaults = { size = 30 }
function start(node) node.x = 999 end -- must NOT fire on an action
function roll(node)
  node:setCelestial{ mu = 5000, parent = "Sun", atmoColor = {0.2, 0.4, 0.9} }
  node:setMaterial{ unlit = true, emissiveStrength = 2 }
  createNode("Child", node, function(c)
c:setTerrain(3)
c:setTerrainGen{ radius = params.size, caveDepth = 12, seed = 99 }
  end)
  terrain.generatePlanet(3, { radius = params.size, caveDepth = 0 })
end
"#,
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, floptle_core::Name("Gen".into()));
    world.insert(e, Matter::Empty);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "gen".into(),
            enabled: true,
            params: vec![("size".into(), 42.0)],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    let ran = host.call_action(&mut world, &dir, e.index(), "gen", "roll");
    assert!(ran, "action failed: {:?}", host.errors());
    // start() must not have fired: the transform is untouched.
    assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 0.0);
    let c = world.get::<floptle_core::CelestialBody>(e).expect("setCelestial inserted");
    assert_eq!(c.mu, 5000.0);
    assert_eq!(c.parent, "Sun");
    assert!((c.atmo_color[2] - 0.9).abs() < 1e-5);
    let m = world.get::<floptle_core::Material>(e).expect("setMaterial inserted");
    assert!(m.unlit);
    assert_eq!(m.emissive_strength, 2.0);
    let creates = host.take_create_requests();
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0].name, "Child");
    assert_eq!(creates[0].parent, Some(e.index()));
    // Mimic the editor's drain (apply_spawn_batch): spawn the node, then run
    // the callback — its construction writes must land immediately (the drain
    // is the last flush an editor action gets; a transform-only flush here
    // left generator planets as Matter::Empty and their generated terrain
    // fields orphaned — "generated field … but no node carries it").
    let mut creates = creates;
    let child = world.spawn();
    world.insert(child, Transform::IDENTITY);
    world.insert(child, floptle_core::Name(creates[0].name.clone()));
    world.insert(child, Matter::Empty);
    let cb = creates.remove(0).cb.expect("create carried its callback");
    host.call_create_callback(&mut world, cb, child);
    match world.get::<Matter>(child) {
        Some(Matter::Terrain { id, .. }) => assert_eq!(*id, 3),
        other => panic!("createNode callback's setTerrain(3) did not land: {other:?}"),
    }
    // setTerrainGen: the genspec lands as a RON PlanetFill that parses back
    // (the G2 on-demand generation contract — the streamer regenerates the
    // body from exactly this string).
    let spec = world
        .get::<floptle_core::TerrainGen>(child)
        .expect("setTerrainGen inserted the genspec");
    let fill: floptle_field::procgen::PlanetFill =
        ron::from_str(&spec.0).expect("genspec parses back to a PlanetFill");
    assert_eq!(fill.radius, 42.0); // Inspector-tuned param reached the spec
    assert_eq!(fill.cave_depth, 12.0);
    assert_eq!(fill.seed, 99);
    let gens = host.take_terrain_generates();
    assert_eq!(gens.len(), 1);
    assert_eq!(gens[0].0, 3);
    // Inspector-tuned params reach the action (42 overrides the default 30).
    assert_eq!(gens[0].1.radius, 42.0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// `lateUpdate` — the camera pass: runs when the driver says (after
/// physics + writeback), sees the frame's dt, can move its node, and
/// never fires before the frame pass `start`ed the instance.
#[test]
fn late_update_runs_after_start_and_moves_the_node() {
    let dir = std::env::temp_dir().join("floptle_script_test_late");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "follow",
        "function update(node, dt)\n  node.y = 5\nend\n\
         function lateUpdate(node, dt)\n  node.x = node.x + dt\nend\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "follow".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    // Before the frame pass builds+starts the instance, lateUpdate is a no-op.
    host.run_late(&mut world, 1.0, 0.0);
    assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 0.0);
    // A normal frame: update runs, then the driver's late pass.
    host.run(&mut world, &dir, 0.5, 0.5);
    host.run_late(&mut world, 0.5, 0.5);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let tr = world.get::<Transform>(e).unwrap();
    assert_eq!(tr.translation.y, 5.0, "update ran");
    assert!((tr.translation.x - 0.5).abs() < 1e-6, "lateUpdate moved the node by dt");
}

#[test]
fn fixed_update_runs_per_tick_with_constant_dt() {
    // The gameplay-tick hook (docs/multiplayer.md §3): `fixedUpdate(node, dt)`
    // runs once per run_fixed call with the constant tick delta, only after the
    // frame pass has started the script, and `update` does not run in the fixed
    // pass (nor fixedUpdate in the frame pass).
    let dir = std::env::temp_dir().join("floptle_script_test_fixed_update");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "ticker",
        "function update(node, dt)\n  node.y = node.y + 1\nend\n\
         function fixedUpdate(node, dt)\n  node.x = node.x + 1\n  node.z = dt\nend\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst { kind: "ticker".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
    );
    let mut host = ScriptHost::new();
    // run_fixed before any frame pass: instance doesn't exist yet → no tick, no error.
    host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 0.0);

    // One frame pass (start + update), then three fixed ticks.
    host.run(&mut world, &dir, 0.016, 0.016);
    for i in 0..3 {
        host.run_fixed(&mut world, 1.0 / 60.0, 0.016 + (i as f32) / 60.0);
    }
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let t = *world.get::<Transform>(e).unwrap();
    // x counts fixedUpdate calls (self-moves write back per tick); y counts updates.
    assert_eq!(t.translation.x, 3.0, "fixedUpdate must run once per run_fixed");
    assert_eq!(t.translation.y, 1.0, "update must run only in the frame pass");
    let want = (1.0f32 / 60.0) as f64;
    assert!((t.translation.z - want).abs() < 1e-9, "fixed dt must be the constant tick delta");
}

#[test]
fn predicted_node_update_rides_the_tick_clock() {
    // The anti-jitter contract (net play-as-client): a frame-filtered
    // entity's `update` is skipped in the per-frame pass and re-run at the
    // tick cadence via run_frame_for — so client and server integrate an
    // update-style controller identically. run_fixed_for also bypasses the
    // filters (it is the substitute execution).
    let dir = std::env::temp_dir().join("floptle_script_test_frame_filter");
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "mover", "function update(node, dt)\n  node.x = node.x + 1\nend\n");
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst { kind: "mover".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.016, 0.016); // start + first update
    assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 1.0);

    let mut fskip = std::collections::HashSet::new();
    fskip.insert(e.index());
    host.set_frame_filter(fskip);
    host.run(&mut world, &dir, 0.016, 0.032); // frame pass: filtered → no move
    assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 1.0);
    host.run_frame_for(&mut world, e.index(), 1.0 / 60.0, 0.048); // tick-cadence update
    assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 2.0);

    host.set_frame_filter(std::collections::HashSet::new());
    host.run(&mut world, &dir, 0.016, 0.064); // cleared → frame pass runs again
    assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 3.0);
}

/// **A script with no hook for this pass is not charged for the frame.**
///
/// Per-script timing is a wall clock around the call, so whatever the
/// machine does inside that span — a garbage collection, the OS taking the
/// core away — is reported as that script's cost. For a script with no
/// `update` at all the span wraps nothing, and a game profiling itself
/// found 12–21 ms "peaks" against a file that could not have spent them.
/// Anybody reading that goes and optimises the wrong file, which is worse
/// than having no per-script numbers at all.
#[test]
fn a_script_with_no_update_is_not_charged_for_the_frame() {
    let dir = std::env::temp_dir().join("floptle_script_test_perf_attrib");
    let _ = std::fs::create_dir_all(&dir);
    // One script that does real work, and one with no hook for this pass at
    // all — the shape that was being blamed.
    write_script(
        &dir,
        "worker",
        "function update(node, dt)\n  local s = 0\n  for i = 1, 20000 do s = s + i end\n               node.y = s * 0.0\nend\n",
    );
    write_script(&dir, "marker", "-- a data-only script: no hooks at all\nfoo = 1\n");

    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![
            floptle_core::ScriptInst {
                kind: "worker".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            },
            floptle_core::ScriptInst {
                kind: "marker".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            },
        ]),
    );
    let mut host = ScriptHost::new();
    host.profile().borrow_mut().enable(true);
    for _ in 0..3 {
        host.run(&mut world, &dir, 0.016, 0.016);
        host.profile().borrow_mut().end_frame();
    }
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());

    let by_script = host.profile().borrow().scripts();
    let named: Vec<&str> = by_script.iter().map(|(n, _)| n.as_str()).collect();
    assert!(named.contains(&"worker"), "the script that did the work is missing: {named:?}");
    assert!(
        !named.contains(&"marker"),
        "a script with no hook was charged for the frame it was not in: {named:?}"
    );
}

/// A script sees its own cost, attributed by file name.
///
/// End to end through the real host, because the value of this API is
/// entirely in a game being able to assert its own budget — and the thing
/// that makes that possible is per-script attribution, which nothing outside
/// `run_pass` can produce.
#[test]
fn a_script_reads_its_own_frame_cost_by_name() {
    let dir = std::env::temp_dir().join(format!("floptle_perf_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    // Two scripts, one deliberately doing more work than the other, so the
    // ordering `perf.scripts()` promises has something to order.
    write_script(
        &dir,
        "busy",
        "\
function start(node)
  perf.enable(true)
end

function update(node, dt)
  local acc = 0
  for i = 1, 200000 do acc = acc + i % 7 end
  spun = acc
end
",
    );
    write_script(&dir, "idle", "function update(node, dt)\n  ticked = true\nend\n");
    let (mut world, _e) = world_with_script("busy");
    let idle = world.spawn();
    world.insert(idle, Transform::IDENTITY);
    world.insert(idle, floptle_core::Name("Idle".into()));
    world.insert(
        idle,
        floptle_core::Scripts(vec![floptle_core::ScriptInst::new("idle")]),
    );
    let mut host = ScriptHost::new();
    // `start` turns collection on; the frames after it are the measured ones.
    for i in 0..4 {
        host.run(&mut world, &dir, 1.0 / 60.0, i as f32 / 60.0);
        host.profile().borrow_mut().end_frame();
    }
    let prof = host.profile().borrow();
    assert!(prof.enabled(), "the script's own perf.enable(true) did not take");
    let rows = prof.scripts();
    assert!(rows.len() >= 2, "both scripts should be listed: {rows:?}");
    assert_eq!(rows[0].0, "busy", "most expensive first: {rows:?}");
    assert!(rows[0].1.ms > 0.0, "the busy script measured as free: {rows:?}");
    // **The host does not write `Bucket::Scripts`.** Since 0.84.2 that bucket
    // is the whole pass, wall-clocked by whoever runs the pass — the editor,
    // around `run`/`run_fixed`/`run_late` — and these rows are the hook time
    // inside it. A host that wrote the bucket too would have the editor's
    // span and its own hook times both in there, counting the pass twice.
    let scripts = prof.bucket(floptle_core::profile::Bucket::Scripts).expect("on");
    assert!(
        scripts.ms.abs() < 1e-6,
        "the host wrote the Scripts bucket ({}), so a real frame counts the pass twice",
        scripts.ms
    );
    // What the host does own is the mirror: `sync_scene` runs three times a
    // frame and had no bucket at all before 0.84.2.
    let mirror = prof.bucket(floptle_core::profile::Bucket::Mirror).expect("on");
    assert!(mirror.ms > 0.0, "the scene mirror reported nothing: {mirror:?}");
    drop(prof);
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A pass the script has no hook for still drains a write made through a
/// stashed handle.** The hook-less fast path skips the params table and
/// the write scan; it must not skip the node — a timer callback writing
/// `me.x = 5` through a handle kept from `start()` has to reach the world on
/// the very next pass, exactly as it did when every pass paid full price.
#[test]
fn a_hookless_pass_still_drains_a_timer_write_to_the_node() {
    let dir = std::env::temp_dir().join(format!("floptle_hookless_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "stasher",
        "local me\n\
         function start(node)\n\
           me = node\n\
           after(0.01, function() me.x = 5 end)\n\
         end\n",
    );
    let (mut world, e) = world_with_script("stasher");
    let mut host = ScriptHost::new();
    // Frame pass: `start` runs, stashes the handle, arms the timer.
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 0.0);
    // Tick pass: the timer fires in the scheduler, before the script pass —
    // and this script has no `fixedUpdate`, so the pass takes the light
    // path. The write must still land.
    host.run_fixed(&mut world, 1.0 / 60.0, 1.0 / 60.0);
    assert_eq!(
        world.get::<Transform>(e).unwrap().translation.x,
        5.0,
        "a hook-less pass dropped a write made through a stashed node handle"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A scene of thousands of scripted nodes runs.
///
/// It used to panic — `out of auxiliary stack space (used 7999 slots)` —
/// because the host held a live `mlua::Table` per instance in two places,
/// and each one costs a slot on a ref stack bounded near 8,000. Two holds
/// put the ceiling around four thousand, which a probe hit and a game
/// eventually would have. Registry keys have no such bound.
///
/// 6,000 because it is comfortably past the old ceiling while staying a
/// second-ish test; `examples/auxstack_probe` runs it to 20,000 and shows
/// which of the two ways of holding a Lua value is the one that runs out.
#[test]
fn thousands_of_scripted_nodes_do_not_exhaust_lua() {
    let dir = std::env::temp_dir().join(format!("floptle_0069_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "prop", "n = 0\nfunction update(node, dt)\n  n = n + 1\nend\n");

    const NODES: usize = 6_000;
    let mut world = World::default();
    let mut last = None;
    for i in 0..NODES {
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, floptle_core::Name(format!("prop{i}")));
        world.insert(e, Scripts(vec![floptle_core::ScriptInst {
            kind: "prop".into(),
            enabled: true,
            params: Vec::new(),
            refs: Vec::new(),
            strs: Vec::new(),
        }]));
        last = Some(e);
    }
    let mut host = ScriptHost::new();
    // Two frames: the first builds every environment, the second proves they
    // are all still reachable — a registry key that was dropped on the way in
    // would read as a script that silently stopped running.
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());

    let last = last.expect("nodes");
    let env = host.instance_env(last.index(), "prop").expect("the LAST instance still resolves");
    assert_eq!(
        env.get::<f64>("n").unwrap(),
        2.0,
        "every instance ran both frames, including the ones past the old ceiling"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A checkbox tunable reads as a real boolean inside a running script —
/// not as the 1/0 it is stored as, and not as a truthy `0`.
///
/// `env::params_table` has done this since the boolean round-trip fix, but
/// only a unit test of that function said so, which is why three shipped
/// examples still carried a private `on(v)` helper to defend against a
/// problem the engine had already solved. This is the end-to-end statement
/// that lets those helpers stay deleted: `if params.thing then` is correct
/// on its own, from a script, with the box unticked.
#[test]
fn an_unticked_checkbox_param_is_false_inside_a_running_script() {
    let dir = std::env::temp_dir().join("floptle_script_test_checkbox");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "box",
        "defaults = { ring = true }\n\
         function update(node, dt)\n\
         \x20 if params.ring then node.x = 1 else node.x = -1 end\n\
         \x20 if type(params.ring) ~= 'boolean' then error('not a boolean: ' .. type(params.ring)) end\n\
         end\n",
    );
    for (stored, want_x) in [(0.0, -1.0), (1.0, 1.0)] {
        let mut world = World::default();
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "box".into(),
                enabled: true,
                params: vec![("ring".into(), stored)],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.016, 0.0);
        assert!(host.errors().is_empty(), "stored={stored}: {:?}", host.errors());
        assert_eq!(
            world.get::<Transform>(e).unwrap().translation.x,
            want_x,
            "a stored {stored} must read as {}",
            stored != 0.0
        );
    }
}

/// The tick-pose channel (`docs/multiplayer.md` §3).
///
/// `node.x` between ticks is the interpolated render pose — lerped by the
/// frame's alpha, so reading it inside `fixedUpdate` is a frame-rate-
/// dependent read that no replay can reproduce, and `node.x = node.x + d`
/// teleports the body onto its visual position (the classic "the visuals
/// take the knockback but the hitbox stays put" bug). `node.tickX/tickY/
/// tickZ/tickPos` are the body's own pose, and writing them moves the body
/// without going near the transform.
#[test]
fn the_tick_pose_channel_reads_and_writes_the_body_not_the_render_transform() {
    let dir = std::env::temp_dir().join("floptle_script_test_tick_pose");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "fighter",
        "function fixedUpdate(node, dt)\n\
           sawX, sawRender = node.tickX, node.x\n\
           sawPos = node.tickPos.y\n\
           if node.tickX < 100 then node.tickX = node.tickX + 5 end\n\
         end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    // The render transform is deliberately somewhere the body is not — that
    // is exactly the situation mid-tick, and the two must not be confused.
    world.insert(e, Transform::from_translation(glam::DVec3::new(-99.0, 0.0, 0.0)));
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "fighter".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.set_bodies(HashMap::from([(
        e.index(),
        BodyState { pos: [10.0, 3.0, -2.0], ..Default::default() },
    )]));
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    host.run_fixed(&mut world, 1.0 / 60.0, 0.0);

    let env = host.instance_env(e.index(), "fighter").unwrap();
    assert_eq!(env.get::<f64>("sawX").unwrap(), 10.0, "tickX is the BODY's pose");
    assert_eq!(env.get::<f64>("sawRender").unwrap(), -99.0, "…and node.x is not");
    assert_eq!(env.get::<f64>("sawPos").unwrap(), 3.0, "tickPos is the same pose as a vec3");

    // The write became a body teleport, not a transform edit.
    let moved = host.take_body_pos_changes();
    assert_eq!(moved.get(&e.index()).copied(), Some([15.0, 3.0, -2.0]));
    assert_eq!(
        world.get::<Transform>(e).unwrap().translation.x,
        -99.0,
        "the render transform must be left exactly alone"
    );
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
}

/// A node with no rigidbody has no tick channel, and saying so beats a
/// silent no-op that looks like a working teleport.
#[test]
fn the_tick_pose_channel_is_absent_without_a_body() {
    let dir = std::env::temp_dir().join("floptle_script_test_tick_pose_nobody");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "prop",
        "function fixedUpdate(node, dt)\n\
           missing = (node.tickPos == nil) and (node.tickX == nil)\n\
           refused = not pcall(function() node.tickX = 5 end)\n\
         end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "prop".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
    let env = host.instance_env(e.index(), "prop").unwrap();
    assert!(env.get::<bool>("missing").unwrap(), "no body, no tick pose");
    assert!(env.get::<bool>("refused").unwrap(), "and writing it is an error, not a no-op");
}

/// **A script with no frame hook still gets its params warnings.** The
/// hook-less fast path (0.84.0) returns before the full setup, and the two
/// `first`-only warnings lived inside the full setup — so a `fixedUpdate`-
/// only controller consumed its first pass on the fast path and a tunable
/// nobody reads went silent. The warning is about the scene's wiring, not
/// about any hook, so it must fire whichever hooks the script has.
#[test]
fn a_fixed_update_only_script_is_warned_about_a_param_it_never_reads() {
    let dir = std::env::temp_dir().join(format!("floptle_fixed_only_params_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "mover",
        "defaults = { speed = 1 }\n\
         function fixedUpdate(node, dt) node.x = node.x + params.speed * dt end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "mover".into(),
            enabled: true,
            params: vec![("speed".into(), 2.0), ("stale".into(), 3.0)],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    host.run_fixed(&mut world, 1.0 / 60.0, 1.0 / 60.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let logs = host.drain_logs();
    let msgs: Vec<&str> = logs.iter().map(|l| l.msg.as_str()).collect();
    assert!(
        msgs.iter().any(|m| m.contains("stale") && m.contains("never read")),
        "no unread-params warning for a fixedUpdate-only script: {msgs:?}"
    );
    assert!(
        !msgs.iter().any(|m| m.contains("speed") && m.contains("never read")),
        "a param the script declares was reported unread: {msgs:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// **Allocation is attributed to the script that made it.** `--alloc` gave
/// a total; a game whose vector was 2% of its per-frame allocation had no
/// way to see where the other 98% came from. Sampled around each hook call
/// while the collector is stopped, which is the only time the difference in
/// heap size means "what this hook allocated".
#[test]
fn allocation_is_attributed_to_the_script_that_made_it() {
    let dir = std::env::temp_dir().join(format!("floptle_alloc_by_script_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "hog",
        "function update(node, dt)\n  local t = {}\n  for i = 1, 2000 do t[i] = { i } end\nend\n",
    );
    write_script(&dir, "lean", "local n = 0\nfunction update(node, dt)\n  n = n + dt\nend\n");
    let mut world = World::default();
    for kind in ["hog", "lean"] {
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
    }
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    host.gc_collect();
    host.gc_stop();
    host.track_alloc(true);
    for i in 1..=10 {
        host.run(&mut world, &dir, 1.0 / 60.0, i as f32 / 60.0);
    }
    let by = host.alloc_by_script();
    host.track_alloc(false);
    host.gc_restart();
    let of = |k: &str| by.iter().find(|(n, _)| n == k).map(|(_, b)| *b).unwrap_or(0);
    let (hog, lean) = (of("hog"), of("lean"));
    // 2000 one-element tables a frame for ten frames, each at least 16
    // bytes. Luau's heap counter moves in 16 KB allocator pages, so the
    // lean script may be charged a page or two that happened to fill on
    // its watch — the bar is "far below", not "nothing".
    assert!(hog > 10 * 2000 * 16, "hog allocated {hog} bytes over ten frames: {by:?}");
    assert!(lean < hog / 10, "lean ({lean}) is not far below hog ({hog}): {by:?}");
    assert_eq!(by.first().map(|(n, _)| n.as_str()), Some("hog"), "not sorted largest first: {by:?}");
    // Off means off: nothing accrues, and the readout is empty again.
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0);
    assert!(host.alloc_by_script().is_empty(), "tracking kept accruing after being turned off");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A hook that does nothing allocates almost nothing.** The 0.84 pass
/// measured Forgery at ~478 KB of Lua heap a frame and read it as the
/// game's own tables; most of it was the engine's. The own-node table is
/// written with `raw_set` and was read back with `Table::get`, and on Luau
/// an mlua `get` against a table that has a metatable — every node table
/// does — takes the protected path and allocates ~96 bytes per key whether
/// or not `__index` is consulted. Ten keys, twice a pass, plus the env
/// writes and the hook lookup, on every scripted node, three passes a
/// frame.
///
/// The marginal cost of one more scripted node, so the per-pass work that
/// is not per-instance (the scene mirror) cannot mask it. Bytes rather than
/// milliseconds: an allocation count is deterministic for a given VM, so
/// this is a ceiling and not the growth ratio a *timing* guard would need.
///
/// Watched failing at 2402.9 B (empty `update`) and 1149.3 B (hook-less)
/// against the 300 below; both read 185.1 after the fix, which is the scene
/// mirror's per-node share and not the hook path.
#[test]
fn a_hook_that_does_nothing_allocates_almost_nothing() {
    let dir = std::env::temp_dir().join(format!("floptle_idle_alloc_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    // An empty `update` takes the full per-pass path; a script with no
    // lifecycle hook at all takes the fast one, which still has to keep the
    // node table live for a handle stashed in `start`.
    write_script(&dir, "bare", "function update(node, dt) end\n");
    write_script(&dir, "nohooks", "local x = 1\n");
    let per_pass = |kind: &str, instances: usize| -> f64 {
        let mut world = World::default();
        for _ in 0..instances {
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
        }
        let mut host = ScriptHost::new();
        // Warm: the first passes build each env, node table and registry
        // key, which are one-offs and not what this measures.
        for i in 0..5 {
            host.run(&mut world, &dir, 1.0 / 60.0, i as f32 / 60.0);
        }
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        host.gc_collect();
        host.gc_stop();
        let before = host.lua_used_memory();
        let passes = 2000;
        for i in 0..passes {
            host.run(&mut world, &dir, 1.0 / 60.0, i as f32 / 60.0);
        }
        let grew = host.lua_used_memory() - before;
        host.gc_restart();
        grew as f64 / passes as f64
    };
    let marginal = |kind: &str| (per_pass(kind, 9) - per_pass(kind, 1)) / 8.0;
    let (bare, nohooks) = (marginal("bare"), marginal("nohooks"));
    assert!(bare < 300.0, "one more empty `update` costs {bare:.1} B of Lua heap per pass");
    assert!(nohooks < 300.0, "one more hook-less script costs {nohooks:.1} B per pass");
    let _ = std::fs::remove_dir_all(&dir);
}
