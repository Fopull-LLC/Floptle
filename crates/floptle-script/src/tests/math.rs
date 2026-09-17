use super::*;

/// **`print(v)` reads the same in both vec3 modes.** A `fast` vector is the
/// VM's own value type, which the deep printer did not know and rendered
/// as `<value>` — bare, and inside every table it was printed in.
#[test]
fn a_fast_vec3_prints_as_a_vec3_and_not_as_a_value() {
    let dir = std::env::temp_dir().join(format!("floptle_fast_print_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "printer",
        "function update(node, dt)\n\
           print(vec3(1, 2, 3))\n\
           print({ p = vec3(4, 5, 6) })\n\
         end\n",
    );
    let (mut world, _e) = world_with_script("printer");
    let mut host = ScriptHost::new();
    host.set_vec3_mode(crate::Vec3Mode::Fast).expect("fast is available on this build");
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let logs = host.drain_logs();
    let msgs: Vec<&str> = logs.iter().map(|l| l.msg.as_str()).collect();
    assert!(msgs.iter().any(|m| m.contains("vec3(1, 2, 3)")), "bare vector: {msgs:?}");
    assert!(msgs.iter().any(|m| m.contains("vec3(4, 5, 6)")), "vector in a table: {msgs:?}");
    assert!(!msgs.iter().any(|m| m.contains("<value>")), "printed as <value>: {msgs:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// vec3/vec2 value types + distance: constructors, operators, methods,
/// node interop (`distance(node, other)`, `node.pos` read/write).
#[test]
fn vector_math_and_distance_work_end_to_end() {
    let dir = std::env::temp_dir().join("floptle_script_test_vecmath");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "vectors",
        "function update(node, dt)\n\
           local score = 0\n\
           local a = vec3(1, 2, 2)\n\
           if a:length() == 3 then score = score + 1 end\n\
           local b = a + vec3(1)\n\
           if b.x == 2 and b.y == 3 and b.z == 3 then score = score + 10 end\n\
           if (a * 2):length() == 6 then score = score + 100 end\n\
           if vec3(2,0,0):normalized() == vec3(1,0,0) then score = score + 1000 end\n\
           if vec3(1,0,0):cross(vec3(0,1,0)).z == 1 then score = score + 10000 end\n\
           if vec3(0,0,0):lerp(vec3(10,0,0), 0.5).x == 5 then score = score + 100000 end\n\
           if distance(vec3(0,0,0), vec3(3,4,0)) == 5 then score = score + 1000000 end\n\
           local target = find(\"Target\")\n\
           if distance(node, target) == 7 then score = score + 10000000 end\n\
           if vec2(3, 4):length() == 5 then score = score + 100000000 end\n\
           node.pos = vec3(score, node.pos.y, 0)\n\
         end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "vectors".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let target = world.spawn();
    world.insert(target, Transform::from_translation(glam::DVec3::new(0.0, 7.0, 0.0)));
    world.insert(target, floptle_core::Name("Target".into()));
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.016, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 111111111.0);
}

/// **A vec3 that cannot cross the wire is refused as a vec3.** Both
/// backings used to fall through to the VM's type name — "userdata can't
/// replicate" for `exact`, "vector can't replicate" for `fast` — neither of
/// which names the thing the author wrote or what to send instead. A plain
/// `{x=, y=, z=}` table is what to send, and it still crosses.
#[test]
fn a_vec3_that_cannot_replicate_is_named_and_the_fix_is_too() {
    for mode in [Vec3Mode::Exact, Vec3Mode::Fast] {
        let lua = mlua::Lua::new();
        crate::math_api::install(&lua).unwrap();
        crate::math_api::set_mode_checked(&lua, mode).unwrap();
        let v: mlua::Value = lua.load("return vec3(1, 2, 3)").eval().unwrap();
        let err = crate::net_api::lua_to_netvalue(&v, 0).expect_err("a vec3 is not a wire value");
        assert!(err.contains("vec3"), "{mode:?}: the refusal does not say vec3: {err}");
        assert!(err.contains("x, y, z"), "{mode:?}: the refusal does not name the fix: {err}");
        let t: mlua::Value = lua.load("return { x = 1, y = 2, z = 3 }").eval().unwrap();
        assert!(crate::net_api::lua_to_netvalue(&t, 0).is_ok(), "{mode:?}: the fix itself was refused");
    }
}

/// **A seeded host rolls the same numbers every run.** `floptle run --seed`
/// exists so two runs of a game that re-randomises its cast are comparable;
/// it has to reach both `math.random` and the no-seed `rng()` form (which
/// otherwise draws from the clock), and consecutive `rng()` calls must still
/// be different streams — a seed that made every `rng()` the same stream
/// would change the game rather than pin it.
#[test]
fn a_seeded_host_rolls_the_same_numbers_every_run() {
    let dir = std::env::temp_dir().join(format!("floptle_seeded_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "roller",
        "function update(node, dt)\n\
           local a, b = rng(), rng()\n\
           print(a:next(), b:next(), math.random(), a.seed, b.seed)\n\
         end\n",
    );
    let roll = |seed: Option<u32>| -> Vec<String> {
        let (mut world, _e) = world_with_script("roller");
        let mut host = ScriptHost::new();
        if let Some(s) = seed {
            host.set_seed(s);
        }
        for i in 0..3 {
            host.run(&mut world, &dir, 1.0 / 60.0, i as f32 / 60.0);
        }
        assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
        host.drain_logs().into_iter().map(|l| l.msg).collect()
    };
    let first = roll(Some(7));
    assert_eq!(first.len(), 3, "{first:?}");
    assert_eq!(first, roll(Some(7)), "two runs with one seed disagreed");
    assert_ne!(first, roll(Some(8)), "two different seeds rolled the same run");
    // Every `rng()` in the run is its own stream: five seeds across the run,
    // no two alike.
    let seeds: Vec<&str> = first.iter().flat_map(|l| l.split_whitespace().skip(3)).collect();
    let mut uniq = seeds.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(uniq.len(), seeds.len(), "seeded rng() streams repeated: {first:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
