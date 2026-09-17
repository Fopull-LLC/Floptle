use super::*;

/// Two files with the same name in different folders: refuse, naming both.
/// Picking one would be a coin flip whose loser is silent.
#[test]
fn an_ambiguous_bare_script_name_is_refused_by_name() {
    let dir = std::env::temp_dir().join("floptle_script_test_kind_ambig");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("a")).unwrap();
    std::fs::create_dir_all(dir.join("b")).unwrap();
    write_script(&dir, "a/thing", "who = \"a\"\n");
    write_script(&dir, "b/thing", "who = \"b\"\n");
    write_script(
        &dir,
        "reader",
        "function update(node, dt)\n  local _ = findScript(\"thing\")\nend\n",
    );
    let mut world = World::default();
    for k in ["a/thing", "b/thing", "reader"] {
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Scripts(vec![floptle_core::ScriptInst::new(k)]));
    }
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);
    let errs = host.errors().join("\n");
    assert!(errs.contains("a/thing") && errs.contains("b/thing"), "{errs}");
}

#[test]
fn captures_print_and_log() {
    let dir = std::env::temp_dir().join("floptle_script_test_logs");
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "talky", "function update(node, dt)\n  log('tick')\n  print('p', 2, true)\nend\n");
    let (mut world, _e) = world_with_script("talky");
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);
    let logs = host.drain_logs();
    assert!(logs.iter().any(|l| l.msg == "tick" && l.level == LogLevel::Debug), "logs: {logs:?}");
    assert!(logs.iter().any(|l| l.msg == "p\t2\ttrue"), "logs: {logs:?}");
    // logs carry the originating script name for jump-to-source.
    assert!(logs.iter().any(|l| l.source.as_ref().is_some_and(|(n, _)| n == "talky")), "no source: {logs:?}");
    assert!(host.drain_logs().is_empty(), "logs should be drained");
}

#[test]
fn captures_errors_in_console_feed() {
    let dir = std::env::temp_dir().join("floptle_script_test_err");
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "broken", "function update(node, dt)\n  this_is_not_defined()\nend\n");
    let (mut world, _e) = world_with_script("broken");
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);
    assert!(!host.errors().is_empty(), "should report an error");
    let logs = host.drain_logs();
    assert!(logs.iter().any(|l| l.level == LogLevel::Error), "expected an error log: {logs:?}");
    assert!(logs.iter().any(|l| l.source.as_ref().is_some_and(|(n, _)| n == "broken")), "error lacks source: {logs:?}");
}

/// A broken script and a script with no such export both read `nil` through
/// a handle. Only one of them is a bug in the caller.
#[test]
fn reading_from_a_script_that_failed_to_load_says_so() {
    let dir = std::env::temp_dir().join(format!("floptle_broken_read_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "radar", "function update(node, dt) end\nthis is not lua\n");
    write_script(
        &dir,
        "hud",
        "function update(node, dt)\n  local h = findScript('radar')\n  if h then local _ = h.target end\nend\n",
    );

    let mut world = World::default();
    for kind in ["radar", "hud"] {
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Scripts(vec![floptle_core::ScriptInst {
            kind: kind.into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]));
    }
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);
    host.run(&mut world, &dir, 0.1, 0.1);

    let logs = host.drain_logs();
    let told = logs
        .iter()
        .find(|l| l.msg.contains("`radar` did not load"))
        .unwrap_or_else(|| panic!("the reader was never told radar is broken: {logs:?}"));
    assert!(told.msg.contains("target"), "names the key it could not answer: {}", told.msg);
    assert!(
        told.msg.contains("not a missing export"),
        "the whole point is the distinction: {}",
        told.msg
    );
}

/// A mistyped hook is the other silent failure — `ui.on(b, "onClicked", …)`
/// would register a listener nothing ever calls. It raises instead.
#[test]
fn a_mistyped_hook_name_raises() {
    let dir = std::env::temp_dir().join("floptle_script_test_ui_on_typo");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "typo",
        "function start(node)\n  ui.on(find(\"Btn1\"), \"onClicked\", function() end)\nend\n",
    );
    let (mut world, _menu, _btns) = menu_world("typo", 1);
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    let errs = format!("{:?}", host.errors());
    assert!(errs.contains("not a UI hook"), "{errs}");
    assert!(errs.contains("clicked"), "it lists the real ones: {errs}");
}

/// A bad table shape passed to a construction API is a script error in the Console,
/// never a process abort. It used to take the whole editor down with SIGABRT
/// ("panic in a function that cannot unwind"), losing unsaved work and telling the
/// author nothing about what they got wrong.
#[test]
fn a_bad_field_shape_is_a_script_error_not_an_abort() {
    let dir = std::env::temp_dir().join("floptle_script_test_bad_field_shape");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "flash",
        "function update(node, dt)\n\
           node:setMaterial{ emissive = { nope = 1 } }\n\
         end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, Matter::Empty);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "flash".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.016, 0.0);
    let errs = host.errors();
    assert!(!errs.is_empty(), "the bad shape must surface as a script error");
    assert!(
        errs[0].contains("flash") && errs[0].contains("emissive"),
        "the error must name the script and the offending field: {errs:?}"
    );
}

/// A script with no hooks is not rolled back and must not be an error — that
/// is the documented default for cosmetics. And a snapshot holding something
/// unrestorable is refused loudly rather than silently dropped, because a
/// state that looks restored and isn't is the worst of both.
#[test]
fn scripts_without_hooks_are_skipped_and_bad_state_is_refused() {
    let dir = std::env::temp_dir().join("floptle_script_test_rollback_optout");
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "cosmetic", "function fixedUpdate(node, dt) end\n");
    write_script(
        &dir,
        "broken",
        "function snapshot() return { cb = function() end } end\n\
         function restore(s) end\n\
         function fixedUpdate(node, dt) end\n",
    );
    let mut world = World::default();
    let plain = world.spawn();
    world.insert(plain, Transform::IDENTITY);
    world.insert(
        plain,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "cosmetic".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let bad = world.spawn();
    world.insert(bad, Transform::IDENTITY);
    world.insert(
        bad,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "broken".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);

    assert!(!host.has_rollback_hooks(plain.index()));
    let s = host.snapshot_scripts(plain.index());
    assert!(s.is_empty(), "no hooks, nothing captured");
    host.restore_scripts(plain.index(), &s); // and restoring is a no-op
    assert!(host.errors().is_empty(), "opting out is not an error: {:?}", host.errors());

    let s = host.snapshot_scripts(bad.index());
    assert!(s.is_empty(), "the unrestorable capture is refused, not stored");
    let errs = host.errors();
    assert!(
        errs.iter().any(|e| e.contains("broken") && e.contains("rolled back")),
        "the error must name the script and say what's wrong: {errs:?}"
    );
}

/// **A script that raises every frame reads its source once.** The runtime
/// error rewriter quotes the offending line from the file, and it read the
/// file per error — one read per instance per pass, sixty nodes on one
/// broken script being 180 reads a frame. The text is kept with the source
/// and dropped when the file changes (the mtime bump that already resets
/// the generation), so the quoted line is never stale either.
#[test]
fn a_script_that_raises_every_frame_reads_its_source_once() {
    let dir = std::env::temp_dir().join(format!("floptle_source_once_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "faulty", "function update(node, dt)\n  node.postion.x = 1\nend\n");
    let (mut world, _e) = world_with_script("faulty");
    let mut host = ScriptHost::new();
    for i in 0..5 {
        host.run(&mut world, &dir, 1.0 / 60.0, i as f32 / 60.0);
    }
    assert!(
        host.errors().iter().any(|e| e.contains("node.postion")),
        "the rewrite stopped quoting the line: {:?}",
        host.errors()
    );
    let reads = host.source_reads("faulty");
    assert_eq!(reads, 1, "five identical errors read the file {reads} times");

    // The file changes: the cached text must go with the old generation, so
    // the quoted line is the new line.
    std::thread::sleep(std::time::Duration::from_millis(20));
    write_script(&dir, "faulty", "function update(node, dt)\n  local a = 1\n  node.psotion.x = a\nend\n");
    let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
    let f = std::fs::File::open(dir.join("faulty.lua")).unwrap();
    f.set_modified(later).unwrap();
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0);
    assert!(
        host.errors().iter().any(|e| e.contains("node.psotion")),
        "the quoted line came from the OLD file: {:?}",
        host.errors()
    );
    assert_eq!(host.source_reads("faulty"), 2, "the new version was not read exactly once");
    let _ = std::fs::remove_dir_all(&dir);
}
