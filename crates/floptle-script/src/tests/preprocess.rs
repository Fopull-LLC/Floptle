use super::*;

#[test]
fn preprocess_rewrites_compound_ops() {
    assert_eq!(preprocess("x += y"), "x = x + (y)");
    assert_eq!(preprocess("tbl.k *= 2"), "tbl.k = tbl.k * (2)");
    assert_eq!(preprocess("a[i] -= f()"), "a[i] = a[i] - (f())");
    assert_eq!(preprocess("s ..= 'z'"), "s = s .. ('z')");
    assert_eq!(preprocess("p %= 3"), "p = p % (3)");
    assert_eq!(preprocess("q ^= 2"), "q = q ^ (2)");
    assert_eq!(preprocess("n /= 2"), "n = n / (2)");
    // Precedence: the whole RHS is parenthesized.
    assert_eq!(preprocess("x *= a + b"), "x = x * (a + b)");
    // Nested index lvalue, balanced brackets.
    assert_eq!(preprocess("a[b[i]] += 1"), "a[b[i]] = a[b[i]] + (1)");
    // Inline block (lvalue back-scan stops at the keyword boundary).
    assert_eq!(preprocess("if c then x += 1 end"), "if c then x = x + (1) end");
}

#[test]
fn preprocess_ignores_strings_and_comments() {
    assert_eq!(preprocess("s = 'x += y'"), "s = 'x += y'");
    assert_eq!(preprocess("-- x += y"), "-- x += y");
    assert_eq!(preprocess("t = [[ a += b ]]"), "t = [[ a += b ]]");
    assert_eq!(preprocess("t = [==[ a += b ]==]"), "t = [==[ a += b ]==]");
    assert_eq!(preprocess("if a == b then end"), "if a == b then end");
    assert_eq!(preprocess("c = a .. b"), "c = a .. b"); // concat untouched
    assert_eq!(preprocess("x = -y"), "x = -y"); // unary minus untouched
}

#[test]
fn preprocess_preserves_line_count() {
    let src = "x += 1\ny -= 2\n-- z += 3\n";
    assert_eq!(preprocess(src).matches('\n').count(), src.matches('\n').count());
}

#[test]
fn preprocess_closes_rhs_at_comments_and_statements() {
    // Trailing comment must not be swallowed into the RHS parentheses.
    assert_eq!(preprocess("x += 1 -- note"), "x = x + (1) -- note");
    assert_eq!(preprocess("s ..= 'z' -- c"), "s = s .. ('z') -- c");
    // A call/parenthesized receiver lvalue is captured whole.
    assert_eq!(preprocess("f().x += 1"), "f().x = f().x + (1)");
    assert_eq!(preprocess("(a).b -= 2"), "(a).b = (a).b - (2)");
    // A statement-introducing keyword on the same line terminates the RHS.
    assert_eq!(
        preprocess("function f() x += 1 return x end"),
        "function f() x = x + (1) return x end"
    );
    assert_eq!(preprocess("while c do n += 1 end"), "while c do n = n + (1) end");
}

#[test]
fn compound_assignment_runs_end_to_end() {
    let dir = std::env::temp_dir().join("floptle_script_test_compound");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "spin",
        "defaults = { speed = 90 }\nfunction update(node, dt)\n  node.yaw += math.rad(params.speed) * dt\nend\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, Scripts(vec![floptle_core::ScriptInst {
        kind: "spin".into(),
        enabled: true,
        params: vec![("speed".into(), 90.0)], refs: Vec::new(),
        strs: Vec::new(),
    }]));
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0, 1.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let tr = world.get::<Transform>(e).unwrap();
    let (yaw, _, _) = tr.rotation.to_euler(EulerRot::YXZ);
    assert!((yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-3, "yaw was {yaw}");
}

/// A script that crosses the VM's upvalue ceiling must be told what
/// happened — **and where there is no ceiling it must
/// simply run.**
///
/// On LuaJIT the raw message is `…:3669: function at line 2864 has more
/// than 60 upvalues` — it names the END of the offending function rather
/// than the reference that tipped it over, never says a limit exists, and
/// arrives from the loader, so the script does not run at all.
/// `vessel_controller` hit this twice, a release apart, on mechanical edits.
///
/// Luau has no such ceiling (ADR-0028; `tests/vm_dialect.rs` measures it
/// rather than quoting it), so the same 70-upvalue file that cost two
/// releases there loads and runs here. That is a real difference between
/// the two VMs and this test states it in both directions, because a
/// `#[cfg]`-skipped test asserts nothing about the VM that skipped it.
#[test]
fn crossing_the_upvalue_ceiling_names_the_script_the_limit_and_the_fix() {
    let dir = std::env::temp_dir().join(format!("floptle_upvalue_over_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    // 70 file-scope locals, and one function that closes over every one of
    // them: exactly the shape one more `local` produces in a long script.
    let mut src = String::new();
    for i in 0..70 {
        src.push_str(&format!("local v{i} = {i}\n"));
    }
    src.push_str("function update(node, dt)\n  local t = 0\n");
    for i in 0..70 {
        src.push_str(&format!("  t = t + v{i}\n"));
    }
    src.push_str("  node.y = t\nend\n");
    write_script(&dir, "huge", &src);

    let (mut world, _e) = world_with_script("huge");
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);

    let Some(limit) = crate::load_error::UPVALUE_LIMIT else {
        // No ceiling: the file is just a file.
        assert!(
            host.errors().is_empty(),
            "this VM has no upvalue ceiling, so a 70-upvalue script must load: {:?}",
            host.errors()
        );
        let logs = host.drain_logs();
        assert!(
            !logs.iter().any(|l| l.level == LogLevel::Error),
            "…and must not report one either: {logs:?}"
        );
        return;
    };
    assert_eq!(limit, 60, "the message below quotes the limit; keep them together");

    let errs = host.errors().to_vec();
    let msg = errs.iter().find(|e| e.contains("huge")).unwrap_or_else(|| {
        panic!("the load failure must be reported: {errs:?}")
    });
    assert!(msg.contains("huge.lua"), "names the script: {msg}");
    assert!(msg.contains("60 upvalues"), "names the limit: {msg}");
    assert!(msg.contains("LuaJIT"), "names whose limit it is: {msg}");
    assert!(msg.contains("local s ="), "names the fix: {msg}");

    // …and once on the Console, not once per frame. A load failure fails
    // every frame; sixty identical lines a second is how a Console feed
    // stops being read.
    let first = host.drain_logs();
    assert_eq!(
        first.iter().filter(|l| l.level == LogLevel::Error).count(),
        1,
        "one Console line for the load failure: {first:?}"
    );
    for _ in 0..5 {
        host.run(&mut world, &dir, 0.1, 0.1);
    }
    let later = host.drain_logs();
    assert!(
        !later.iter().any(|l| l.level == LogLevel::Error),
        "the same failure must not re-print every frame: {later:?}"
    );
    assert!(
        !host.errors().is_empty(),
        "…but the Scripting tab still lists it as currently broken"
    );
}

/// One edit from the wall, the engine says so — because the count is
/// invisible from inside the editor, and crossing it costs the whole script.
///
/// Where there is no wall (Luau — ADR-0028) the engine must say **nothing**.
/// A warning about a limit that is not there is worse than silence: it sends
/// somebody to restructure a working script for no reason.
#[test]
fn a_script_near_the_upvalue_ceiling_is_warned_before_it_crosses() {
    let dir = std::env::temp_dir().join(format!("floptle_upvalue_near_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let mut src = String::new();
    for i in 0..55 {
        src.push_str(&format!("local v{i} = {i}\n"));
    }
    src.push_str("function update(node, dt)\n  local t = 0\n");
    for i in 0..55 {
        src.push_str(&format!("  t = t + v{i}\n"));
    }
    src.push_str("  node.y = t\nend\n");
    write_script(&dir, "nearly", &src);

    let (mut world, _e) = world_with_script("nearly");
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);

    assert!(host.errors().is_empty(), "it still LOADS: {:?}", host.errors());
    let logs = host.drain_logs();
    if crate::load_error::UPVALUE_LIMIT.is_none() {
        assert!(
            !logs.iter().any(|l| l.msg.contains("upvalues")),
            "this VM has no upvalue ceiling, so nothing should warn about one: {logs:?}"
        );
        return;
    }
    let warn = logs
        .iter()
        .find(|l| l.level == LogLevel::Warn && l.msg.contains("upvalues"))
        .unwrap_or_else(|| panic!("expected an upvalue-pressure warning: {logs:?}"));
    assert!(warn.msg.contains("nearly.lua"), "{}", warn.msg);
    assert!(warn.msg.contains("55 file-scope locals"), "names the count: {}", warn.msg);
    assert!(warn.msg.contains("5 to go"), "names the headroom: {}", warn.msg);
    assert!(warn.msg.contains("before the script stops loading"), "{}", warn.msg);

    // Once per version of the file, not once a frame.
    for _ in 0..5 {
        host.run(&mut world, &dir, 0.1, 0.1);
    }
    assert!(
        !host.drain_logs().iter().any(|l| l.msg.contains("upvalues")),
        "the warning repeats every frame"
    );
}
