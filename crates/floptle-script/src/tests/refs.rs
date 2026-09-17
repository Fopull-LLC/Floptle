use super::*;

/// A script filed in a subfolder must answer to the name on its tab.
///
/// This is the bug as reported: `scripts/forgery/playermovement.lua` is
/// stored on the node as the kind `forgery/playermovement`, because a kind
/// is a path under `scripts/` without the extension. Nothing a person looks
/// at says so — the editor tab says `playermovement.lua`, the Inspector row
/// says `playermovement`, the Console prefixes its output `playermovement:22`
/// — so `node:getscript("playermovement")` is what everybody writes, and it
/// matched nothing and returned `nil` with no complaint. The `nil` then
/// surfaced two scripts away as a value that was never set.
#[test]
fn a_script_in_a_subfolder_answers_to_its_bare_name() {
    let dir = std::env::temp_dir().join("floptle_script_test_kind_stem");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("forgery")).unwrap();
    write_script(&dir, "forgery/playermovement", "state = \"idle\"\n");
    write_script(
        &dir,
        "reader",
        "function update(node, dt)\n  \
         local m = findTagged(\"Player\")[1]:getscript(\"playermovement\")\n  \
         if m and m.state == \"idle\" then node.x = 1 end\n\
         end\n",
    );

    let mut world = World::default();
    let player = world.spawn();
    world.insert(player, Transform::IDENTITY);
    world.insert(player, floptle_core::Tags(vec!["Player".into()]));
    world.insert(
        player,
        Scripts(vec![floptle_core::ScriptInst::new("forgery/playermovement")]),
    );
    let reader = world.spawn();
    world.insert(reader, Transform::IDENTITY);
    world.insert(reader, Scripts(vec![floptle_core::ScriptInst::new("reader")]));

    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(
        world.get::<Transform>(reader).unwrap().translation.x,
        1.0,
        "the bare file name must reach a script filed in a folder"
    );
}

/// …and the full path still means exactly what it meant, so a project that
/// already spells them out is untouched.
#[test]
fn the_full_script_path_still_resolves() {
    let dir = std::env::temp_dir().join("floptle_script_test_kind_path");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("forgery")).unwrap();
    write_script(&dir, "forgery/mover", "state = 7\n");
    write_script(
        &dir,
        "reader",
        "function update(node, dt)\n  \
         local m = findScript(\"forgery/mover\")\n  \
         if m then node.x = m.state end\n\
         end\n",
    );
    let mut world = World::default();
    let a = world.spawn();
    world.insert(a, Transform::IDENTITY);
    world.insert(a, Scripts(vec![floptle_core::ScriptInst::new("forgery/mover")]));
    let b = world.spawn();
    world.insert(b, Transform::IDENTITY);
    world.insert(b, Scripts(vec![floptle_core::ScriptInst::new("reader")]));
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(world.get::<Transform>(b).unwrap().translation.x, 7.0);
}

/// A `getscript` that finds nothing says what the node does carry — once,
/// however many frames poll it.
#[test]
fn a_getscript_miss_names_what_the_node_carries() {
    let dir = std::env::temp_dir().join("floptle_script_test_getscript_miss");
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "held", "hp = 1\n");
    write_script(
        &dir,
        "reader",
        "function update(node, dt)\n  local _ = node:getscript(\"helth\")\nend\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, floptle_core::Name("Hero".into()));
    world.insert(
        e,
        Scripts(vec![
            floptle_core::ScriptInst::new("held"),
            floptle_core::ScriptInst::new("reader"),
        ]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);
    let said: Vec<String> = host
        .drain_logs()
        .into_iter()
        .filter(|l| l.level == LogLevel::Warn)
        .map(|l| l.msg)
        .collect();
    assert_eq!(said.len(), 1, "exactly one line: {said:?}");
    assert!(said[0].contains("Hero") && said[0].contains("held"), "{}", said[0]);
    // Three more frames of the same miss must stay at one line.
    host.run(&mut world, &dir, 0.1, 0.2);
    host.run(&mut world, &dir, 0.1, 0.3);
    assert!(
        host.drain_logs().iter().all(|l| l.level != LogLevel::Warn),
        "a miss polled every frame is still one Console line"
    );
}

/// Every other node method is camelCase (`hasTag`, `setWorldPos`,
/// `distanceTo`), so `node:getChild` / `node:getParent` / `node:getScript`
/// is what gets written — and all three used to die at the call with
/// "attempt to call method 'getChild' (a nil value)", which names the
/// symptom and nothing to do about it.
#[test]
fn the_get_node_methods_take_the_camel_case_spelling() {
    let dir = std::env::temp_dir().join("floptle_script_test_camel_get");
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "kid", "hp = 3\n");
    write_script(
        &dir,
        "reader",
        "function update(node, dt)\n  \
         local k = node:getChild(\"Kid\")\n  \
         if k and k:getParent().name == \"Root\" then node.x = k:getScript(\"kid\").hp end\n\
         end\n",
    );
    let mut world = World::default();
    let root = world.spawn();
    world.insert(root, Transform::IDENTITY);
    world.insert(root, floptle_core::Name("Root".into()));
    world.insert(root, Scripts(vec![floptle_core::ScriptInst::new("reader")]));
    let kid = world.spawn();
    world.insert(kid, Transform::IDENTITY);
    world.insert(kid, floptle_core::Name("Kid".into()));
    world.insert(kid, floptle_core::Parent(root));
    world.insert(kid, Scripts(vec![floptle_core::ScriptInst::new("kid")]));
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(world.get::<Transform>(root).unwrap().translation.x, 3.0);
}

/// A CASING slip on any other node method names its fix rather than dying
/// at the call site. Genuinely unknown keys still read nil, so a feature
/// probe (`if node.someday then`) keeps working.
#[test]
fn a_node_method_casing_slip_names_the_fix() {
    let dir = std::env::temp_dir().join("floptle_script_test_node_casing");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "typo",
        "function update(node, dt)\n  \
         if dt < 0.15 then\n    if node.someday == nil then node.y = 4 end\n  \
         else\n    node.x = node:HasTag(\"x\") and 1 or 0\n  end\n\
         end\n",
    );
    let (mut world, e) = world_with_script("typo");
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);
    assert!(host.errors().is_empty(), "a nil probe must not raise: {:?}", host.errors());
    assert_eq!(world.get::<Transform>(e).unwrap().translation.y, 4.0);
    host.run(&mut world, &dir, 0.2, 0.3);
    let errs = host.errors().join("\n");
    assert!(errs.contains("did you mean `hasTag`"), "{errs}");
}

/// A script that is attached but switched off reads nil through a handle,
/// exactly like a live script with no such export. They want completely
/// different fixes, so the handle says which one it is.
#[test]
fn reading_a_switched_off_script_says_it_is_switched_off() {
    let dir = std::env::temp_dir().join("floptle_script_test_off_script");
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "engine", "power = 9\n");
    write_script(
        &dir,
        "reader",
        "function update(node, dt)\n  local _ = node:getscript(\"engine\").power\nend\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![
            floptle_core::ScriptInst { enabled: false, ..floptle_core::ScriptInst::new("engine") },
            floptle_core::ScriptInst::new("reader"),
        ]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);
    let said = host
        .drain_logs()
        .into_iter()
        .filter(|l| l.level == LogLevel::Warn)
        .map(|l| l.msg)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(said.contains("attached but not running"), "{said}");
}

/// A cross-script `h.name(...)` calls the script's own function.
///
/// This is the exact shape a player reported as "the commerce center is
/// still just erroring": `materials.lua` exported `function name(id)`
/// returning a display name — the obvious spelling — and the handle answered
/// `name` itself, with the script's own kind, as a string. Every caller died
/// at the call site with `attempt to call field 'name' (a string value)`,
/// and only at the moment it had something to display, so the mining
/// readout, the pickup line, the depot stock list and the research panel
/// each broke separately.
#[test]
fn a_script_that_exports_name_can_be_called_by_other_scripts() {
    let dir = std::env::temp_dir().join(format!("floptle_shadow_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "materials",
        "\
function name(id)
  return id == 'iron' and 'Iron Ore' or '?'
end
",
    );
    write_script(
        &dir,
        "readout",
        "\
function update(node, dt)
  local m = findScript('materials')
  label = m.name('iron')
  which = m.kind
  live = m.valid
end
",
    );
    let (mut world, e) = world_with_script("readout");
    let mats = world.spawn();
    world.insert(mats, Transform::IDENTITY);
    world.insert(mats, floptle_core::Name("Materials".into()));
    world.insert(
        mats,
        floptle_core::Scripts(vec![floptle_core::ScriptInst::new("materials")]),
    );
    let mut host = ScriptHost::new();
    // Two frames: the readout may reach the materials handle only once both
    // instances have been ensured.
    for i in 0..2 {
        host.run(&mut world, &dir, 1.0 / 60.0, i as f32 / 60.0);
    }
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let env = host.instance_env(e.index(), "readout").expect("the readout ran");
    assert_eq!(
        env.get::<String>("label").ok().as_deref(),
        Some("Iron Ore"),
        "the handle answered `name` itself instead of calling the script's function"
    );
    // …and the two keys that are the handle's still work, so nothing lost the
    // ability to ask which script a handle is or whether it is still loaded.
    assert_eq!(env.get::<String>("which").ok().as_deref(), Some("materials"));
    assert_eq!(env.get::<bool>("live").ok(), Some(true));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A script exporting a name the handle does keep is reported at load, once,
/// naming the script and the key.
#[test]
fn exporting_a_reserved_handle_key_is_reported_at_load() {
    let dir = std::env::temp_dir().join(format!("floptle_shadow2_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "stock",
        "\
function kind(id)
  return 'ore'
end

function update(node, dt)
  ran = true
end
",
    );
    let (mut world, _e) = world_with_script("stock");
    let mut host = ScriptHost::new();
    for i in 0..3 {
        host.run(&mut world, &dir, 1.0 / 60.0, i as f32 / 60.0);
    }
    let logs = host.drain_logs();
    let warns: Vec<&crate::ScriptLog> = logs
        .iter()
        .filter(|l| l.level == crate::LogLevel::Warn && l.msg.contains("findScript handle"))
        .collect();
    assert_eq!(warns.len(), 1, "one line per script per session: {warns:?}");
    let msg = &warns[0].msg;
    for want in ["stock", "`kind`", "which script this is"] {
        assert!(msg.contains(want), "missing {want:?}: {msg}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// **Anything that is not a transform forces a full rebuild** — here a
/// rename, the kind of change the refresh path cannot see and must never
/// be allowed to hide.
#[test]
fn a_rename_forces_a_full_rebuild_and_find_sees_it() {
    let dir = std::env::temp_dir().join(format!("floptle_mirror_rename_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "seeker", "function update(node, dt) node.y = find('Renamed') and 1 or 0 end\n");
    let (mut world, e) = world_with_script("seeker");
    let other = world.spawn();
    world.insert(other, Transform::IDENTITY);
    world.insert(other, floptle_core::Name("Other".into()));
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert_eq!(world.get::<Transform>(e).unwrap().translation.y, 0.0, "not renamed yet");
    crate::host::FULL_SYNCS.with(|c| c.set(0));

    world.get_mut::<floptle_core::Name>(other).unwrap().0 = "Renamed".into();
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    assert_eq!(crate::host::FULL_SYNCS.with(|c| c.get()), 1, "a rename must rebuild the mirror once");
    assert_eq!(world.get::<Transform>(e).unwrap().translation.y, 1.0, "find() did not see the new name");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The kind/tag index behind `findScript` has to answer
/// exactly what the scan answered — first in scene order — and it has to
/// keep answering it after the scene changes. A stale index handing back a
/// dead handle would be worse than the scan it replaced.
#[test]
fn the_script_index_answers_in_scene_order_and_follows_the_scene() {
    let dir = std::env::temp_dir().join(format!("floptle_0063_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "manager",
        "defaults = { id = 0 }\nfunction start(node)\n  myId = params.id\nend\n",
    );
    write_script(
        &dir,
        "probe",
        "\
function update(node, dt)
  local one = findScript('manager')
  log(string.format('%d %d %d', one and one.myId or -1,
                #findScripts('manager'), #findTagged('crew')))
end
",
    );
    let (mut world, _driver) = world_with_script("probe");
    let mut managers = Vec::new();
    for i in 0..3 {
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, floptle_core::Name(format!("m{i}")));
        world.insert(e, floptle_core::Tags(vec!["crew".into()]));
        world.insert(e, Scripts(vec![floptle_core::ScriptInst {
            kind: "manager".into(),
            enabled: true,
            params: vec![("id".into(), i as f32)],
            refs: Vec::new(),
            strs: Vec::new(),
        }]));
        managers.push(e);
    }
    let mut host = ScriptHost::new();
    // The script logs `first all tagged`; the last line is this pass's.
    let answer = |host: &mut ScriptHost, world: &mut World, t: f32| -> (i32, i32, i32) {
        host.run(world, &dir, 1.0 / 60.0, t);
        assert!(host.errors().is_empty(), "{:?}", host.errors());
        let last = host.drain_logs().pop().expect("the probe logged");
        let n: Vec<i32> = last.msg.split_whitespace().map(|s| s.parse().unwrap()).collect();
        (n[0], n[1], n[2])
    };
    // One pass to build every environment and seed its params; the reads
    // that matter come after.
    let _ = answer(&mut host, &mut world, 0.0);
    assert_eq!(
        answer(&mut host, &mut world, 1.0 / 60.0),
        (0, 3, 3),
        "the FIRST manager in scene order, and all three found"
    );

    // Despawn the first one: the index must follow, and the answer becomes
    // the next in order rather than a handle to something that is gone.
    // Despawn the first: the index must follow. which survivor answers is
    // the ECS column's business (a despawn swaps the last row into the
    // hole, and the scan this replaced read the same order) — the
    // guarantee is that it is never the dead one.
    world.despawn(managers[0]);
    let (first, all, tagged) = answer(&mut host, &mut world, 2.0 / 60.0);
    assert!(first == 1 || first == 2, "a SURVIVING manager, not the despawned one: {first}");
    assert_eq!((all, tagged), (2, 2), "the index followed the despawn");

    // …and a script removed in the Inspector stops being found, while the
    // node itself (and its tag) stays.
    // A script removed in the Inspector stops being found; the node and
    // its tag stay.
    world.insert(managers[1], Scripts(Vec::new()));
    let (first, all, tagged) = answer(&mut host, &mut world, 3.0 / 60.0);
    assert_eq!((first, all, tagged), (2, 1, 2), "one manager script left, both tags stay");

    // Nothing at all: an empty answer, not a stale one.
    world.insert(managers[2], Scripts(Vec::new()));
    assert_eq!(
        answer(&mut host, &mut world, 4.0 / 60.0),
        (-1, 0, 2),
        "an empty answer, not a stale one"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn node_hierarchy_traversal() {
    let dir = std::env::temp_dir().join("floptle_script_test_hier");
    let _ = std::fs::create_dir_all(&dir);
    // A child reads its parent's x (+1) and finds a sibling by name.
    write_script(
        &dir,
        "reader",
        "function update(node, dt)\n  local p = node.parent\n  if p then node.x = p.x + 1 end\nend\n",
    );
    let mut world = World::default();
    let parent = world.spawn();
    world.insert(
        parent,
        Transform::from_translation(floptle_core::math::DVec3::new(10.0, 0.0, 0.0)),
    );
    world.insert(parent, floptle_core::Name("Parent".into()));
    let child = world.spawn();
    world.insert(child, Transform::IDENTITY);
    world.insert(child, floptle_core::Parent(parent));
    world.insert(child, floptle_core::Name("Child".into()));
    world.insert(
        child,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "reader".into(),
            enabled: true,
            params: vec![], refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.016, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    // child.x = parent.x + 1 = 11 (local transforms, like the `node` argument).
    assert!(
        (world.get::<Transform>(child).unwrap().translation.x - 11.0).abs() < 1e-6,
        "child.x = {}",
        world.get::<Transform>(child).unwrap().translation.x
    );
}

/// `node.worldX/Y/Z` compose the parent chain: a unit under a moved,
/// rotated, scaled container has to be able to answer "where am I, really?"
/// — comparing a local x against a world-space order is how a click-to-move
/// script walks off into the distance and never arrives.
#[test]
fn world_position_composes_the_parent_chain() {
    let dir = std::env::temp_dir().join("floptle_script_test_worldpos");
    let _ = std::fs::create_dir_all(&dir);
    // The script checks itself and raises on a mismatch — the host reports
    // a Lua error, which is what this test reads.
    write_script(
        &dir,
        "probe",
        "function update(node, dt)\n\
         \x20 local wx, wz = node.worldX, node.worldZ\n\
         \x20 if math.abs(wx - 10.0) > 1e-4 or math.abs(wz + 10.0) > 1e-4 then\n\
         \x20   error('world ' .. wx .. ',' .. wz)\n\
         \x20 end\n\
         \x20 if math.abs(node.x - 3.0) > 1e-6 then error('local x moved') end\n\
         end\n",
    );
    let mut world = World::default();
    let parent = world.spawn();
    let mut pt = Transform::from_translation(floptle_core::math::DVec3::new(10.0, 1.0, -4.0));
    pt.rotation = floptle_core::math::Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
    pt.scale = floptle_core::math::Vec3::splat(2.0);
    world.insert(parent, pt);
    let child = world.spawn();
    world.insert(
        child,
        Transform::from_translation(floptle_core::math::DVec3::new(3.0, 0.0, 0.0)),
    );
    world.insert(child, floptle_core::Parent(parent));
    world.insert(
        child,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "probe".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    // Parent local +X, scaled 2 and yawed 90°, lands 6 along −Z: (10, 1, −10).
    let want = floptle_core::world_transform(&world, child).translation;
    assert!((want.x - 10.0).abs() < 1e-6 && (want.z + 10.0).abs() < 1e-6, "{want:?}");
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.016, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
}

/// The local ↔ world set, against the frame that breaks a naive
/// implementation: a parent that is moved, ROTATED and SCALED.
///
/// `node:setWorldPos` and `node:moveTowards` go back through
/// `Transform::inv_mul` rather than decomposing a matrix — the componentwise
/// TRS inverse, whose doc comment explains why the matrix route puts a
/// mirrored parent's negative determinant on the wrong axis. The mirrored
/// case is the last assertion here, and it is the one that would silently
/// pass with the wrong maths on an un-mirrored parent.
#[test]
fn local_and_world_conversions_survive_a_rotated_scaled_parent() {
    let dir = std::env::temp_dir().join("floptle_script_test_localworld");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "probe",
        "function update(node, dt)\n\
         \x20 local function near(a, b, what)\n\
         \x20   if math.abs(a - b) > 1e-4 then error(what .. ': ' .. a .. ' ~= ' .. b) end\n\
         \x20 end\n\
         \x20 -- toWorld/toLocal round trip through the whole chain.\n\
         \x20 local back = node:toLocal(node:toWorld(vec3(1, 2, 3)))\n\
         \x20 near(back.x, 1, 'toLocal x') near(back.y, 2, 'toLocal y') near(back.z, 3, 'toLocal z')\n\
         \x20 -- The node's own origin in world space is node.worldPos.\n\
         \x20 local o = node:toWorld(vec3(0, 0, 0))\n\
         \x20 near(o.x, node.worldX, 'origin x') near(o.z, node.worldZ, 'origin z')\n\
         \x20 -- worldForward composes the parent's rotation; node.forward does not.\n\
         \x20 near(node:worldForward():length(), 1, 'forward is unit')\n\
         \x20 near(node:worldForward().x, -1, 'parent yaw 90 turns -Z into -X')\n\
         \x20 -- setWorldPos: ask for a world point, land on it.\n\
         \x20 node:setWorldPos(vec3(2, 7, -3))\n\
         \x20 near(node.worldX, 2, 'set x') near(node.worldY, 7, 'set y') near(node.worldZ, -3, 'set z')\n\
         \x20 -- distanceTo/Flat are WORLD measurements.\n\
         \x20 near(node:distanceTo(vec3(2, 7, -3)), 0, 'distanceTo self')\n\
         \x20 near(node:distanceFlat(vec3(2, 99, -3)), 0, 'distanceFlat ignores up')\n\
         \x20 near(node:distanceTo(vec3(2, 99, -3)), 92, 'distanceTo does not')\n\
         \x20 -- moveTowards steps in world space and never overshoots.\n\
         \x20 node:moveTowards(vec3(2, 7, 7), 4)\n\
         \x20 near(node.worldZ, 1, 'moveTowards stepped 4 of 10')\n\
         \x20 local arrived = node:moveTowards(vec3(2, 7, 7), 999)\n\
         \x20 near(node.worldZ, 7, 'moveTowards landed exactly')\n\
         \x20 if not arrived then error('moveTowards should report arrival') end\n\
         end\n",
    );
    // Two runs: an ordinary parent, then a MIRRORED one (negative Y scale).
    for mirror in [false, true] {
        let mut world = World::default();
        let parent = world.spawn();
        let mut pt =
            Transform::from_translation(floptle_core::math::DVec3::new(10.0, 1.0, -4.0));
        pt.rotation = floptle_core::math::Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
        pt.scale = if mirror {
            floptle_core::math::Vec3::new(2.0, -1.5, 2.0)
        } else {
            floptle_core::math::Vec3::splat(2.0)
        };
        world.insert(parent, pt);
        let child = world.spawn();
        world.insert(
            child,
            Transform::from_translation(floptle_core::math::DVec3::new(3.0, 0.5, 0.0)),
        );
        world.insert(child, floptle_core::Parent(parent));
        world.insert(
            child,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "probe".into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 0.016, 0.0);
        assert!(
            host.errors().is_empty(),
            "mirror={mirror} errors: {:?}",
            host.errors()
        );
    }
}

/// `node:lookAt` and `node:turnTowards` — the two names that replace an
/// `atan2` with two minus signs and a shortest-arc dance across the ±π seam.
#[test]
fn look_at_faces_the_target_and_turn_towards_takes_the_short_way() {
    let dir = std::env::temp_dir().join("floptle_script_test_lookat");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "aim",
        "function update(node, dt)\n\
         \x20 local function near(a, b, what)\n\
         \x20   if math.abs(a - b) > 1e-4 then error(what .. ': ' .. a .. ' ~= ' .. b) end\n\
         \x20 end\n\
         \x20 -- Straight down -Z is yaw 0; the target is a plain world point.\n\
         \x20 node:lookAt(vec3(0, 0, -10))\n\
         \x20 near(node.yaw, 0, 'yaw at a -Z target')\n\
         \x20 near(node.pitch, 0, 'pitch at a level target')\n\
         \x20 -- A NODE handle aims at where that node WORLD is.\n\
         \x20 node:lookAt(find('Target'))\n\
         \x20 near(node.yaw, math.pi / 2, 'yaw at a -X target')\n\
         \x20 -- Looking up: +Y target, pitch positive.\n\
         \x20 node:lookAt(vec3(0, 10, -10))\n\
         \x20 near(node.pitch, math.pi / 4, 'pitch at a raised target')\n\
         \x20 -- turnTowards steps by at most maxRadians, the SHORT way across\n\
         \x20 -- the seam: from -170 deg toward +170 deg is +20, not -340.\n\
         \x20 node.yaw = math.rad(-170)\n\
         \x20 node.pitch = 0\n\
         \x20 node:turnTowards(dirFromYaw(math.rad(170)), math.rad(5))\n\
         \x20 near(math.deg(node.yaw), -175, 'turnTowards went the long way')\n\
         \x20 -- A big enough step lands exactly on the target angle.\n\
         \x20 node:turnTowards(dirFromYaw(math.rad(170)), math.pi)\n\
         \x20 near(math.abs(math.deg(node.yaw)), 170, 'turnTowards should arrive')\n\
         \x20 -- A zero direction leaves the facing alone (no NaN, no snap).\n\
         \x20 local was = node.yaw\n\
         \x20 node:turnTowards(vec3(0, 0, 0), 1)\n\
         \x20 near(node.yaw, was, 'a zero direction must not move the facing')\n\
         end\n",
    );
    let mut world = World::default();
    let target = world.spawn();
    world.insert(
        target,
        Transform::from_translation(floptle_core::math::DVec3::new(-10.0, 0.0, 0.0)),
    );
    world.insert(target, floptle_core::Name("Target".into()));
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "aim".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.016, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
}

#[test]
fn cross_script_reference_method_and_state() {
    let dir = std::env::temp_dir().join("floptle_script_test_xref");
    let _ = std::fs::create_dir_all(&dir);
    // A manager holds state + a method; the method moves its own node via `node`.
    write_script(
        &dir,
        "manager",
        "score = 0\nfunction addScore(n)\n  score = score + n\n  node.x = score\nend\nfunction update(node, dt) end\n",
    );
    // A ticker finds the manager anywhere in the scene and calls its method.
    write_script(
        &dir,
        "ticker",
        "function update(node, dt)\n  local m = findScript('manager')\n  if m then m.addScore(5) end\nend\n",
    );
    let mut world = World::default();
    let mgr = world.spawn();
    world.insert(mgr, Transform::IDENTITY);
    world.insert(mgr, floptle_core::Name("Manager".into()));
    world.insert(
        mgr,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "manager".into(),
            enabled: true,
            params: vec![], refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let t = world.spawn();
    world.insert(t, Transform::IDENTITY);
    world.insert(
        t,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "ticker".into(),
            enabled: true,
            params: vec![], refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    for _ in 0..3 {
        host.run(&mut world, &dir, 0.016, 0.0);
    }
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    // 3 frames × +5 = 15; the manager moved itself to x = score via its node handle.
    assert!(
        (world.get::<Transform>(mgr).unwrap().translation.x - 15.0).abs() < 1e-6,
        "manager.x = {}",
        world.get::<Transform>(mgr).unwrap().translation.x
    );
}

#[test]
fn noderef_param_resolves_to_a_handle_and_rebinds_by_name() {
    // defaults = { target = noderef() } + an Inspector-wired name -> the script
    // sees a node handle in params (no find()); unwired refs read nil.
    let dir = std::env::temp_dir().join("floptle_script_test_noderef");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "aimer",
        concat!(
            "defaults = { target = noderef(), missing = noderef(), speed = 2 }\n",
            "function update(node, dt)\n",
            "  if params.target then params.target.y = 5 end\n",
            "  node.x = (params.missing == nil and 1 or 0) + params.speed\n",
            "end\n",
        ),
    );
    let mut world = World::default();
    let driver = world.spawn();
    world.insert(driver, Transform::IDENTITY);
    world.insert(
        driver,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "aimer".into(),
            enabled: true,
            params: vec![],
            refs: vec![
                ("target".into(), "Turret".into()),
                ("missing".into(), String::new()),
            ],
            strs: Vec::new(),
        }]),
    );
    let turret = world.spawn();
    world.insert(turret, Transform::IDENTITY);
    world.insert(turret, floptle_core::Name("Turret".into()));
    let mut host = ScriptHost::new();
    // The defaults surface reports the ref params for the Inspector.
    let path = dir.join("aimer.lua");
    let (nums, refs, _strs) = host.script_defaults(&path);
    assert_eq!(
        refs,
        vec![
            ("missing".to_string(), RefKind::Node),
            ("target".to_string(), RefKind::Node)
        ]
    );
    assert_eq!(nums, vec![("speed".to_string(), 2.0)]);
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(world.get::<Transform>(turret).unwrap().translation.y, 5.0);
    // missing == nil (1) + speed (2): the sentinel never leaks as a string.
    assert_eq!(world.get::<Transform>(driver).unwrap().translation.x, 3.0);
}

/// **A reference param follows the scene, not the wire.** `params.target`
/// is resolved by name, so a target that does not exist yet at the first
/// frame must appear in `params` when it spawns, and vanish when it is
/// renamed away — without the Inspector touching the wire. The per-hook
/// rebuild of `params` used to give this for free; the fingerprinted
/// rebuild has to earn it, and this is where it is watched.
#[test]
fn noderef_param_rebinds_when_the_target_appears_or_is_renamed_mid_play() {
    let dir = std::env::temp_dir().join(format!("floptle_noderef_live_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "seeker",
        "defaults = { target = noderef() }\n\
         function update(node, dt) node.x = params.target and 1 or 0 end\n",
    );
    let mut world = World::default();
    let driver = world.spawn();
    world.insert(driver, Transform::IDENTITY);
    world.insert(
        driver,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "seeker".into(),
            enabled: true,
            params: vec![],
            refs: vec![("target".into(), "Turret".into())],
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(world.get::<Transform>(driver).unwrap().translation.x, 0.0, "no Turret yet");

    // The target spawns mid-play, the way a streamed level or a prefab does.
    let turret = world.spawn();
    world.insert(turret, Transform::IDENTITY);
    world.insert(turret, floptle_core::Name("Turret".into()));
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    assert_eq!(
        world.get::<Transform>(driver).unwrap().translation.x,
        1.0,
        "a target that spawned after the first frame never reached params"
    );

    // …and a rename takes it away again.
    world.get_mut::<floptle_core::Name>(turret).unwrap().0 = "Decoy".into();
    host.run(&mut world, &dir, 1.0 / 60.0, 2.0 / 60.0);
    assert_eq!(
        world.get::<Transform>(driver).unwrap().translation.x,
        0.0,
        "a renamed target stayed bound under its old name"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scriptref_and_componentref_bind_handles_directly() {
    // scriptref("health") gives the wired node's health script handle;
    // componentref("RigidBody") gives its component handle; a wire to a node
    // MISSING the declared thing reads nil (validated, not a dead handle).
    let dir = std::env::temp_dir().join("floptle_script_test_kindrefs");
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "health", "hp = 40\nfunction damage(n)\n  hp = hp - n\nend\n");
    write_script(
        &dir,
        "attacker",
        concat!(
            "defaults = { victim = scriptref(\"health\"), body = componentref(\"RigidBody\"),\n",
            "             bogus = componentref(\"PointLight\") }\n",
            "function update(node, dt)\n",
            "  if params.victim then params.victim.damage(15) end\n",
            "  if params.body then params.body.friction = 0.05 end\n",
            "  node.x = (params.bogus == nil) and 1 or 0\n",
            "end\n",
        ),
    );
    let mut world = World::default();
    let attacker = world.spawn();
    world.insert(attacker, Transform::IDENTITY);
    world.insert(
        attacker,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "attacker".into(),
            enabled: true,
            params: vec![],
            refs: vec![
                ("victim".into(), "Dummy".into()),
                ("body".into(), "Dummy".into()),
                ("bogus".into(), "Dummy".into()), // Dummy has no PointLight → nil
            ],
            strs: Vec::new(),
        }]),
    );
    let dummy = world.spawn();
    world.insert(dummy, Transform::IDENTITY);
    world.insert(dummy, floptle_core::Name("Dummy".into()));
    world.insert(dummy, RigidBody::default());
    world.insert(
        dummy,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "health".into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    // The health script's state took the damage call.
    let hp: f64 = host.instance_env(dummy.index(), "health").unwrap().get("hp").unwrap();
    assert_eq!(hp, 25.0);
    assert_eq!(world.get::<RigidBody>(dummy).unwrap().friction, 0.05);
    assert_eq!(world.get::<Transform>(attacker).unwrap().translation.x, 1.0);
}

/// `me = node` kept from `start()` must read the current pose on later hooks.
///
/// It used to freeze at the spawn position: `node_table` built a fresh table per
/// hook with x/y/z as raw fields, so the stashed reference was a snapshot. It failed
/// silently and only partially — everything using the passed `node` stayed correct,
/// so a character moved and animated fine while anything derived from the stashed
/// handle (hitboxes, hand-anchored effects) stayed nailed to the spawn point.
#[test]
fn a_handle_kept_from_start_tracks_the_node() {
    let dir = std::env::temp_dir().join("floptle_script_test_stashed_handle");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "walker",
        "function start(node) me = node end\n\
         function update(node, dt)\n\
           seen = me.x          -- read the STASHED handle, before this frame's move\n\
           node.x = node.x + 1\n\
         end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "walker".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    for i in 0..3 {
        host.run(&mut world, &dir, 0.016, i as f32 * 0.016);
    }
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 3.0, "the body walked");
    let seen = host
        .instance_env(e.index(), "walker")
        .and_then(|env| env.get::<f64>("seen").ok())
        .unwrap_or(f64::NAN);
    assert_eq!(seen, 2.0, "the stashed handle must track the body, not freeze at spawn");
}

/// A write through a stashed handle from outside that script's hooks — the shape a
/// cross-script `other:knockBack()` takes — lands. It used to be dropped: the write
/// arrived after the target's read-back had drained, and the next hook's re-stamp
/// overwrote it.
#[test]
fn a_cross_script_write_through_a_stashed_handle_lands() {
    let dir = std::env::temp_dir().join("floptle_script_test_cross_script_write");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "target",
        "function start(node) me = node end\n\
         function teleport(x) me.x = x end\n\
         function update(node, dt) end\n",
    );
    write_script(
        &dir,
        "caller",
        "function update(node, dt)\n\
           if not done then\n\
             find(\"Target\"):getscript(\"target\").teleport(-5)\n\
             done = true\n\
           end\n\
         end\n",
    );
    let mut world = World::default();
    let target = world.spawn();
    world.insert(target, Transform::IDENTITY);
    world.insert(target, floptle_core::Name("Target".into()));
    world.insert(
        target,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "target".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let caller = world.spawn();
    world.insert(caller, Transform::IDENTITY);
    world.insert(caller, floptle_core::Name("Caller".into()));
    world.insert(
        caller,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "caller".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    for i in 0..4 {
        host.run(&mut world, &dir, 0.016, i as f32 * 0.016);
    }
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(
        world.get::<Transform>(target).unwrap().translation.x,
        -5.0,
        "the teleport written through the stashed handle must reach the transform"
    );
}
