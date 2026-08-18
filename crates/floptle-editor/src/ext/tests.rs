//! Tests for the extension host: load a package from a temp folder, run its
//! Lua, and check what it registered and queued.
//!
//! These deliberately go through [`ExtHost::reload`] rather than poking the
//! bindings directly — the thing worth testing is that a folder on disk becomes
//! a working extension, which is the whole path a package author walks.

use std::path::{Path, PathBuf};

use super::*;

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "flext-{name}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Write a package into `<proj>/packages/<id>` with `main.lua`, and register it.
fn install(proj: &Path, id: &str, permissions: &str, lua: &str) {
    let root = proj.join("packages").join(id);
    std::fs::create_dir_all(root.join("editor")).unwrap();
    std::fs::write(
        root.join("package.ron"),
        format!(
            r#"( id: "{id}", name: "{id}", version: "1.0.0", permissions: [{permissions}] )"#
        ),
    )
    .unwrap();
    std::fs::write(root.join("editor/main.lua"), lua).unwrap();
    let mut reg = floptle_package::Registry::load(proj).unwrap();
    reg.upsert(floptle_package::Entry {
        id: id.into(),
        version: "1.0.0".parse().unwrap(),
        source: floptle_package::Source::Authored,
        enabled: true,
    });
    reg.save(proj).unwrap();
}

fn engine() -> floptle_package::Version {
    floptle_package::Version::new(0, 55, 0)
}

fn host_for(proj: &Path) -> ExtHost {
    let mut host = ExtHost::new();
    // A project root has to be in the snapshot before anything reads it.
    host.begin_frame(
        Snapshot { project_root: proj.to_path_buf(), ..Snapshot::default() },
        SceneMirror::default(),
    );
    host.reload(proj, &engine());
    host
}

#[test]
fn a_package_registers_a_dock_tab_and_its_key_survives_a_reload() {
    let proj = temp("tabreg");
    install(
        &proj,
        "com.t.tab",
        "",
        r#"
        local t = ed.tab("Settings", function() end)
        ed.menu("T/Settings", function() t:show() end)
        "#,
    );
    let mut host = host_for(&proj);
    assert!(host.packages[0].failed.is_none(), "{:?}", host.packages[0].failed);
    assert_eq!(host.tabs.len(), 1);
    assert_eq!(host.tabs[0].title, "Settings");
    // A tab arrives CLOSED. One that opened itself would rearrange the user's
    // dock every time the project opened.
    assert_eq!(host.shared.open_state.borrow().get(&host.tabs[0].id).copied(), Some(false));

    // The key is what a saved layout holds, so it has to be the same number
    // after a reload — the runtime id is not.
    let before = host.tabs[0].key;
    host.reload(&proj, &engine());
    assert_eq!(host.tabs.len(), 1);
    assert_eq!(host.tabs[0].key, before, "a reload must not move the tab");
    assert_eq!(host.tab_title(before), Some("Settings"));

    // Two packages may both call a tab "Settings" without docking on top of
    // each other.
    assert_ne!(super::tab_key("com.t.tab", "Settings"), super::tab_key("com.other", "Settings"));
    assert_ne!(super::tab_key("com.t.tab", "Settings"), super::tab_key("com.t.tab", "Chat"));
}

#[test]
fn a_package_registers_a_panel_a_menu_and_a_hook() {
    let proj = temp("register");
    install(
        &proj,
        "com.t.a",
        "",
        r#"
        local w = ed.window("Tools", function() end)
        ed.menu("Tools/Open", function() w:show() end)
        ed.onSceneDraw(function() end)
        ed.shortcut("ctrl+l", function() end)
        "#,
    );
    let host = host_for(&proj);
    assert!(host.packages[0].failed.is_none(), "{:?}", host.packages[0].failed);
    assert_eq!(host.windows.len(), 1);
    assert_eq!(host.windows[0].title, "Tools");
    assert_eq!(host.menus.len(), 1);
    assert_eq!(host.menus[0].path, "Tools/Open");
    assert_eq!(host.hooks.len(), 1);
    assert_eq!(host.shortcuts[0].keys, "Ctrl+L");
    let _ = std::fs::remove_dir_all(&proj);
}

/// The permission story, checked from Lua's side: an undeclared capability is
/// ABSENT, so `http` is nil rather than a function that refuses.
#[test]
fn an_undeclared_capability_is_absent_not_refused() {
    let proj = temp("perm");
    install(
        &proj,
        "com.t.a",
        "",
        r#"
        ed.log("http is " .. type(http))
        ed.log("sys is " .. type(sys))
        ed.log("io is " .. type(io))
        ed.log("os.execute is " .. type(os.execute))
        "#,
    );
    let host = host_for(&proj);
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert!(log.contains(&"http is nil".to_string()), "{log:?}");
    assert!(log.contains(&"sys is nil".to_string()), "{log:?}");
    assert!(log.contains(&"io is nil".to_string()), "{log:?}");
    assert!(log.contains(&"os.execute is nil".to_string()), "{log:?}");
    let _ = std::fs::remove_dir_all(&proj);
}

/// A package must be able to hash something. `bit` is pure arithmetic with no
/// route to the host, so it is on the allow-list — and this is also the check
/// that the allow-list does not quietly lose an entry.
#[test]
fn the_environment_has_what_a_hash_needs() {
    let proj = temp("bitlib");
    install(
        &proj,
        "com.t.a",
        "",
        r#"
        -- The sandbox has no `_G`, so a package probes for an optional global
        -- by reading it: an unknown name is simply nil.
        ed.log(type(bit), type(_G))
        if bit then
            ed.log(tostring(bit.band(0xF0, 0x3C)), tostring(bit.tobit(0xFFFFFFFF)))
        end
        "#,
    );
    let host = host_for(&proj);
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert_eq!(log[0], "table\tnil");
    assert_eq!(log[1], "48\t-1");
    let _ = std::fs::remove_dir_all(&proj);
}

#[test]
fn a_declared_capability_is_there() {
    let proj = temp("perm2");
    install(&proj, "com.t.a", "Network, Browser", r#"ed.log(type(http), type(sys))"#);
    let host = host_for(&proj);
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert_eq!(log, vec!["table\ttable".to_string()]);
    let _ = std::fs::remove_dir_all(&proj);
}

/// A package with a syntax error must not stop the editor, or the package
/// beside it.
#[test]
fn one_broken_package_does_not_take_the_others_with_it() {
    let proj = temp("broken");
    install(&proj, "com.t.good", "", r#"ed.window("Fine", function() end)"#);
    install(&proj, "com.t.bad", "", "this is not lua ===");
    let host = host_for(&proj);
    assert_eq!(host.windows.len(), 1);
    let bad = host.packages.iter().find(|p| p.id == "com.t.bad").unwrap();
    assert!(bad.failed.is_some());
    let good = host.packages.iter().find(|p| p.id == "com.t.good").unwrap();
    assert!(good.failed.is_none());
    let _ = std::fs::remove_dir_all(&proj);
}

#[test]
fn a_hook_that_raises_is_reported_once_and_stops_being_called() {
    let proj = temp("raise");
    install(
        &proj,
        "com.t.a",
        "",
        r#"
        calls = 0
        ed.onUpdate(function() calls = calls + 1; error("nope") end)
        "#,
    );
    let mut host = host_for(&proj);
    host.take_log();
    host.fire(HookKind::Update);
    host.fire(HookKind::Update);
    host.fire(HookKind::Update);
    let errors = host.take_log();
    assert_eq!(errors.len(), 1, "{:?}", errors.iter().map(|e| &e.msg).collect::<Vec<_>>());
    assert!(errors[0].msg.contains("onUpdate"), "{}", errors[0].msg);
    let _ = std::fs::remove_dir_all(&proj);
}

#[test]
fn scene_reads_come_from_the_mirror_and_edits_become_commands() {
    let proj = temp("scene");
    install(
        &proj,
        "com.t.a",
        "",
        r#"
        ed.onUpdate(function()
            local ids = scene.all()
            for _, id in ipairs(ids) do
                local n = scene.info(id)
                if n.name == "Door" then
                    scene.setPos(id, vec3(1, 2, 3))
                    scene.setName(id, "Opened")
                end
            end
        end)
        "#,
    );
    let mut host = host_for(&proj);

    let mut w = floptle_core::World::new();
    let e = w.spawn();
    w.insert(e, floptle_core::Name("Door".into()));
    w.insert(e, floptle_core::Matter::Empty);
    host.begin_frame(
        Snapshot { project_root: proj.clone(), ..Snapshot::default() },
        SceneMirror::build(&w, &|_, _| None, &|_, _| None),
    );
    host.fire(HookKind::Update);

    let cmds = host.take_cmds();
    assert!(
        cmds.iter().any(|c| matches!(c, ExtCmd::NodeSetPos(id, p)
            if *id == e.index() && *p == [1.0, 2.0, 3.0])),
        "{cmds:?}"
    );
    assert!(cmds.iter().any(|c| matches!(c, ExtCmd::NodeSetName(_, n) if n == "Opened")));
    let _ = std::fs::remove_dir_all(&proj);
}

#[test]
fn handles_queue_world_space_lines_and_clear_each_frame() {
    let proj = temp("handles");
    install(
        &proj,
        "com.t.a",
        "",
        r#"
        ed.onSceneDraw(function()
            handles.color(1, 0, 0)
            handles.line(vec3(0,0,0), vec3(0,1,0))
            handles.wireCube(vec3(0,0,0), vec3(2,2,2))
        end)
        "#,
    );
    let mut host = host_for(&proj);
    host.fire(HookKind::SceneDraw);
    // one line + twelve cube edges
    assert_eq!(host.handles().len(), 13);
    let first = host.handles()[0].clone();
    match first {
        HandleCmd::Line { a, b, color, .. } => {
            assert_eq!(a, [0.0, 0.0, 0.0]);
            assert_eq!(b, [0.0, 1.0, 0.0]);
            assert_eq!(color, [1.0, 0.0, 0.0, 1.0]);
        }
        other => panic!("{other:?}"),
    }
    // A frame with no drawing leaves nothing behind.
    host.begin_frame(Snapshot::default(), SceneMirror::default());
    assert_eq!(host.handles().len(), 0);
    let _ = std::fs::remove_dir_all(&proj);
}

#[test]
fn the_three_stores_persist_the_two_that_should() {
    let proj = temp("stores");
    install(
        &proj,
        "com.t.a",
        "",
        r#"
        ed.prefs.set("key", "abc")
        ed.store.set("n", 4)
        ed.session.set("flag", true)
        ed.log(tostring(ed.prefs.get("key")), tostring(ed.store.get("n")),
               tostring(ed.session.get("flag")), tostring(ed.store.get("absent", "fallback")))
        "#,
    );
    let host = host_for(&proj);
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert_eq!(log, vec!["abc\t4\ttrue\tfallback".to_string()]);
    host.save_prefs();
    assert!(proj.join(".floptle/packages/com.t.a.ron").exists());
    let _ = std::fs::remove_dir_all(&proj);
}

/// A store holds scalars; anything structured goes through `json.encode`. The
/// error has to say so rather than silently dropping the value.
#[test]
fn a_store_refuses_a_table_and_says_what_to_do() {
    let proj = temp("storetable");
    install(
        &proj,
        "com.t.a",
        "",
        r#"
        local ok, err = pcall(function() ed.store.set("k", { a = 1 }) end)
        ed.log(tostring(ok), tostring(err))
        "#,
    );
    let host = host_for(&proj);
    let log = host.take_log();
    assert!(log[0].msg.starts_with("false"), "{}", log[0].msg);
    assert!(log[0].msg.contains("json.encode"), "{}", log[0].msg);
    let _ = std::fs::remove_dir_all(&proj);
}

#[test]
fn json_round_trips_a_table() {
    let proj = temp("json");
    install(
        &proj,
        "com.t.a",
        "",
        r#"
        local s = json.encode({ name = "a", n = 2, list = {1, 2, 3} })
        local back = json.decode(s)
        ed.log(back.name, tostring(back.n), tostring(#back.list), tostring(back.list[3]))
        "#,
    );
    let host = host_for(&proj);
    assert_eq!(host.take_log()[0].msg, "a\t2\t3\t3");
    let _ = std::fs::remove_dir_all(&proj);
}

/// `json.null` writes a null; `nil` removes the key.
///
/// The distinction is the whole reason the sentinel exists — an API that reads
/// an absent field as "leave this alone" and an explicit null as "clear it"
/// cannot be told the second thing otherwise. The decode half is asserted too,
/// because the asymmetry is deliberate and an accidental "fix" of it would turn
/// every `if body.field then` in every package the wrong way round.
#[test]
fn json_null_writes_a_null_where_nil_removes_the_key() {
    let proj = temp("jsonnull");
    install(
        &proj,
        "com.t.a",
        "",
        r#"
        local s = json.encode({ cleared = json.null, kept = 1, absent = nil })
        ed.log(s)
        -- Round trip: a null comes back as nil, so the key reads as absent.
        local back = json.decode('{"a":null,"b":2}')
        ed.log(tostring(back.a), tostring(back.b), tostring(back.a == nil))
        ed.log(tostring(json.null == json.null), tostring(json.null == nil))
        "#,
    );
    let host = host_for(&proj);
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert!(log[0].contains("\"cleared\":null"), "{}", log[0]);
    assert!(log[0].contains("\"kept\":1"), "{}", log[0]);
    assert!(!log[0].contains("absent"), "a nil field must not appear at all: {}", log[0]);
    assert_eq!(log[1], "nil\t2\ttrue");
    assert_eq!(log[2], "true\tfalse");
    let _ = std::fs::remove_dir_all(&proj);
}

#[test]
fn a_package_can_require_its_own_files_and_nothing_else() {
    let proj = temp("require");
    install(
        &proj,
        "com.t.a",
        "",
        r#"
        local helper = require("lib/helper")
        ed.log(helper.greet())
        local ok, err = pcall(function() return require("../../../etc/passwd") end)
        ed.log(tostring(ok))
        "#,
    );
    let root = proj.join("packages/com.t.a");
    std::fs::create_dir_all(root.join("lib")).unwrap();
    std::fs::write(root.join("lib/helper.lua"), "return { greet = function() return \"hi\" end }")
        .unwrap();
    let host = host_for(&proj);
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert_eq!(log, vec!["hi".to_string(), "false".to_string()]);
    let _ = std::fs::remove_dir_all(&proj);
}

/// `editor/` is scanned recursively, so `lib/helper.lua` under it would run as
/// a top-level file too. Keeping required files OUT of `editor/` is the
/// author's job — but a file that both runs and is required must not run twice.
#[test]
fn requiring_the_same_file_twice_runs_it_once() {
    let proj = temp("requiretwice");
    install(
        &proj,
        "com.t.a",
        "",
        r#"
        local a = require("lib/counter")
        local b = require("lib/counter")
        ed.log(tostring(a.n), tostring(b.n), tostring(a == b))
        "#,
    );
    let root = proj.join("packages/com.t.a");
    std::fs::create_dir_all(root.join("lib")).unwrap();
    std::fs::write(root.join("lib/counter.lua"), "COUNT = (COUNT or 0) + 1\nreturn { n = COUNT }")
        .unwrap();
    let host = host_for(&proj);
    assert_eq!(host.take_log()[0].msg, "1\t1\ttrue");
    let _ = std::fs::remove_dir_all(&proj);
}

#[test]
fn reading_a_file_outside_the_package_needs_the_files_permission() {
    let proj = temp("files");
    std::fs::write(proj.join("project.ron"), "()").unwrap();
    install(
        &proj,
        "com.t.a",
        "",
        r#"
        ed.log(tostring(ed.read("own.txt")))
        local ok = pcall(function() return ed.read("project.ron") end)
        ed.log(tostring(ok))
        local ok2 = pcall(function() return ed.write("out.txt", "x") end)
        ed.log(tostring(ok2))
        "#,
    );
    std::fs::write(proj.join("packages/com.t.a/own.txt"), "mine").unwrap();
    let host = host_for(&proj);
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert_eq!(log, vec!["mine".to_string(), "false".to_string(), "false".to_string()]);
    let _ = std::fs::remove_dir_all(&proj);
}

/// `ed.readBytes` reads bytes where `ed.read` reads text, and a file the user
/// picked is readable wherever it lives.
///
/// Both halves are the point. `ed.read` is `read_to_string`, so a PNG comes back
/// as **nil** — the same answer as for a file that is not there, which is the
/// most confusing available way to be unreachable. And `ed.pickFile` returns
/// paths from anywhere on the machine, so without the grant the picker can only
/// tell a package the name of a file it cannot open.
#[test]
fn read_bytes_reads_binary_and_honours_what_the_user_picked() {
    let proj = temp("readbytes");
    std::fs::write(proj.join("project.ron"), "()").unwrap();

    // Somewhere the package has no business reaching on its own.
    let outside = std::env::temp_dir().join(format!("floptle-picked-{}.bin", std::process::id()));
    // Bytes that are deliberately not valid UTF-8, which is what makes this a
    // different function rather than a convenience.
    std::fs::write(&outside, [0xffu8, 0x00, 0xfe, b'h', b'i']).unwrap();
    let outside_s = outside.to_string_lossy().to_string();

    install(
        &proj,
        "com.t.a",
        "Files",
        &format!(
            r#"
        local PICKED = "{}"
        -- Not granted yet: the picker has not run. `Files` is held, and it is
        -- still refused — the permission scopes to the PROJECT, and this is not
        -- in it.
        local ok = pcall(function() return ed.readBytes(PICKED) end)
        ed.log("before", tostring(ok))
        -- The package's own folder still works.
        ed.log("own", tostring(#ed.readBytes("own.bin")))
        -- Ask for it properly, and read it inside the callback — which is where
        -- a package would naturally read it, and so the grant has to be in place
        -- by then rather than a frame later.
        ed.pickFile({{ title = "t" }}, function(paths)
            local got = ed.readBytes(paths[1])
            ed.log("after", tostring(got and #got), tostring(got and got:byte(1)))
        end)
        "#,
            outside_s.replace('\\', "\\\\")
        ),
    );
    std::fs::write(proj.join("packages/com.t.a/own.bin"), [0u8, 1, 2, 3]).unwrap();

    let mut host = host_for(&proj);
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert_eq!(log[0], "before\tfalse", "an unpicked path outside the project is refused");
    assert_eq!(log[1], "own\t4", "bytes come back whole, NUL included");

    // Answer the picker exactly as the editor does.
    let reqs = host.take_pick_requests();
    assert_eq!(reqs.len(), 1, "the package asked for a picker");
    host.deliver_pick(reqs.into_iter().next().unwrap().cb, vec![outside_s.clone()]);
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert_eq!(log[0], "after\t5\t255", "and a picked file is readable, bytes intact");

    let _ = std::fs::remove_file(&outside);
    let _ = std::fs::remove_dir_all(&proj);
}

#[test]
fn with_the_files_permission_a_package_can_write_into_the_project() {
    let proj = temp("files2");
    install(
        &proj,
        "com.t.a",
        "Files",
        r#"
        ed.write("out/notes.txt", "hello")
        local ok = pcall(function() return ed.write("../escape.txt", "x") end)
        ed.log(tostring(ok))
        "#,
    );
    let host = host_for(&proj);
    assert_eq!(host.take_log()[0].msg, "false");
    assert_eq!(std::fs::read_to_string(proj.join("out/notes.txt")).unwrap(), "hello");
    assert!(!proj.parent().unwrap().join("escape.txt").exists());
    let _ = std::fs::remove_dir_all(&proj);
}

/// Reloading must not leave the old copy of a file's callbacks running beside
/// the new one.
#[test]
fn a_reload_replaces_everything_the_package_registered() {
    let proj = temp("reload");
    install(&proj, "com.t.a", "", r#"ed.window("One", function() end)"#);
    let mut host = host_for(&proj);
    assert_eq!(host.windows.len(), 1);
    std::fs::write(
        proj.join("packages/com.t.a/editor/main.lua"),
        r#"ed.window("Two", function() end)"#,
    )
    .unwrap();
    host.reload(&proj, &engine());
    assert_eq!(host.windows.len(), 1);
    assert_eq!(host.windows[0].title, "Two");
    let _ = std::fs::remove_dir_all(&proj);
}

#[test]
fn an_assets_only_package_loads_with_no_lua_at_all() {
    let proj = temp("assetsonly");
    let root = proj.join("packages/com.t.art");
    std::fs::create_dir_all(root.join("assets")).unwrap();
    std::fs::write(
        root.join("package.ron"),
        r#"( id: "com.t.art", name: "Art", version: "1.0.0" )"#,
    )
    .unwrap();
    let mut reg = floptle_package::Registry::load(&proj).unwrap();
    reg.upsert(floptle_package::Entry {
        id: "com.t.art".into(),
        version: "1.0.0".parse().unwrap(),
        source: floptle_package::Source::Authored,
        enabled: true,
    });
    reg.save(&proj).unwrap();
    let host = host_for(&proj);
    assert_eq!(host.packages.len(), 1);
    assert!(host.packages[0].failed.is_none());
    assert!(host.windows.is_empty() && host.menus.is_empty());
    let _ = std::fs::remove_dir_all(&proj);
}

#[test]
fn a_panel_handle_tracks_whether_it_is_open() {
    let proj = temp("handle");
    install(
        &proj,
        "com.t.a",
        "",
        r#"
        W = ed.window("P", function() end)
        ed.onUpdate(function() ed.log(tostring(W:isOpen())) end)
        "#,
    );
    let mut host = host_for(&proj);
    host.fire(HookKind::Update);
    assert_eq!(host.take_log()[0].msg, "false");
    let idx = host.window_index(host.windows[0].id).unwrap();
    host.set_window_open(idx, true);
    host.fire(HookKind::Update);
    assert_eq!(host.take_log()[0].msg, "true");
    let _ = std::fs::remove_dir_all(&proj);
}

/// `gui.*` must not exist outside a draw callback — an extension that stashed a
/// widget function would otherwise be drawing into a layout that has ended.
#[test]
fn gui_is_absent_outside_a_draw_callback() {
    let proj = temp("gui");
    install(
        &proj,
        "com.t.a",
        "",
        r#"
        ed.log("at load: " .. type(gui))
        ed.onUpdate(function() ed.log("in a hook: " .. type(gui)) end)
        "#,
    );
    let mut host = host_for(&proj);
    host.fire(HookKind::Update);
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert_eq!(log, vec!["at load: nil".to_string(), "in a hook: nil".to_string()]);
    let _ = std::fs::remove_dir_all(&proj);
}

#[test]
fn a_disabled_package_does_not_run() {
    let proj = temp("disabled");
    install(&proj, "com.t.a", "", r#"ed.window("P", function() end)"#);
    floptle_package::install::set_enabled(&proj, "com.t.a", false).unwrap();
    let host = host_for(&proj);
    assert!(host.packages.is_empty());
    assert!(host.windows.is_empty());
    assert_eq!(host.report.disabled.len(), 1);
    let _ = std::fs::remove_dir_all(&proj);
}

#[test]
fn a_shortcut_that_is_not_one_raises_where_the_author_will_see_it() {
    let proj = temp("badshortcut");
    install(&proj, "com.t.a", "", r#"ed.shortcut("ctrl", function() end)"#);
    let host = host_for(&proj);
    assert!(host.packages[0].failed.is_some());
    assert!(host.packages[0].failed.as_ref().unwrap().contains("shortcut"));
    let _ = std::fs::remove_dir_all(&proj);
}

/// Draw `host`'s first panel through a real egui pass and return what its Lua
/// logged. This is the only way to exercise `gui.*`: it exists for the length of
/// one callback and is bound to a live `Ui`, so a test that does not draw cannot
/// see it at all.
fn draw_once(host: &mut ExtHost, which: usize) {
    let ctx = egui::Context::default();
    // The same stack the editor gives it, package faces included — otherwise a
    // test draws against a font set the editor never has, and `gui.font` is
    // exercised only on its fallback path.
    ctx.set_fonts(crate::fonts::definitions(&host.fonts));
    let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
        host.draw_window(which, ui);
    });
}

/// Draw one package window inside a ui of exactly `width`, and answer how wide
/// the content actually came out.
///
/// The width is the whole point: a spacer that claims the room that is left has
/// nothing to overflow until there is a definite edge to overflow past.
/// ONE context across all the frames, as the editor has. A fresh context per
/// frame is a fresh memory, which quietly hides anything that spans frames —
/// and a ratchet is precisely a thing that spans frames.
fn draw_bounded(host: &mut ExtHost, which: usize, width: f32, frames: usize) -> Vec<f32> {
    let ctx = egui::Context::default();
    ctx.set_fonts(crate::fonts::definitions(&host.fonts));
    let mut out = Vec::new();
    for _ in 0..frames {
        let mut got = 0.0;
        let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.allocate_ui(egui::vec2(width, 400.0), |inner| {
                host.draw_window(which, inner);
                got = inner.min_rect().width();
            });
        });
        out.push(got);
    }
    out
}

/// **A flexible spacer must not grow the panel it is in.**
///
/// Claiming `available_width()` is the obvious way to write one and it is wrong
/// the moment anything follows it: the trailing widget lands past the right
/// edge, egui reads the overflow back as the size the panel wants, and — since
/// that size only ever grows — the panel is wider next frame with the same room
/// to overflow again. That is a window that walks out to the full width of the
/// screen while somebody watches, and never comes back.
#[test]
fn a_flexible_spacer_pushes_right_without_pushing_the_panel_wider() {
    let proj = temp("guiflex");
    install(
        &proj,
        "com.t.flex",
        "",
        r#"
        ed.window("P", function()
            gui.horizontal(function()
                gui.small("left")
                gui.flexibleSpace()
                local at = gui.cursor()
                ed.log(("gap %.0f"):format(300 - (at.x + gui.measure("right", 12).w)))
                gui.small("right")
            end)
        end)
        "#,
    );
    let mut host = host_for(&proj);
    let idx = host.window_index(host.windows[0].id).unwrap();
    host.set_window_open(idx, true);

    // Three frames: the ratchet needs more than one to show, and a fix that
    // only holds for the first frame is not a fix.
    let widths = draw_bounded(&mut host, idx, 300.0, 3);
    assert!(host.windows[idx].error.is_none(), "{:?}", host.windows[idx].error);
    for (i, w) in widths.iter().enumerate() {
        assert!(
            *w <= 300.5,
            "frame {i} laid out {w} wide inside 300 — that overflow is what makes the \
             panel grow: {widths:?}"
        );
    }

    // And it still does its job: by the second frame the trailing label sits
    // against the right edge, which is the whole reason to write one.
    let gaps: Vec<f32> = host
        .take_log()
        .into_iter()
        .filter_map(|l| l.msg.strip_prefix("gap ").and_then(|n| n.parse::<f32>().ok()))
        .collect();
    assert_eq!(gaps.len(), 3, "one reading per frame");
    // Frame one has no measurement yet and claims nothing — deliberately, since
    // a spacer that guessed would overflow once, and once is all a ratchet needs.
    assert!(gaps[0] > 100.0, "first frame should not have guessed: {gaps:?}");
    assert!(
        gaps[1].abs() <= 8.0 && gaps[2].abs() <= 8.0,
        "from the second frame the trailing label should sit at the right edge: {gaps:?}"
    );
    let _ = std::fs::remove_dir_all(&proj);
}

/// **A remembered width belongs to a row, and rows move.**
///
/// A row's id comes from its position among its siblings, so a list that gains
/// or loses one hands a row the measurement taken for a different row. Trusting
/// it overflows by the difference — and `Resize` banks an overflow permanently,
/// which is the whole failure this measurement exists to avoid.
#[test]
fn a_row_that_changed_does_not_reuse_another_rows_measurement() {
    let proj = temp("guiflexmoved");
    install(
        &proj,
        "com.t.flexmoved",
        "",
        r#"
        local frame = 0
        ed.window("P", function()
            frame = frame + 1
            -- BOTH ends change width every other frame, which is what a list
            -- whose rows shift does to the row ids underneath it: the id now
            -- names a different row, and the width remembered for it describes
            -- something else. Trusting the trailing half is what overflows.
            local lead  = (frame % 2 == 0) and "left" or "a much much longer left label"
            local trail = (frame % 2 == 0) and "a much much longer right label" or "right"
            gui.horizontal(function()
                gui.small(lead)
                gui.flexibleSpace()
                gui.small(trail)
            end)
        end)
        "#,
    );
    let mut host = host_for(&proj);
    let idx = host.window_index(host.windows[0].id).unwrap();
    host.set_window_open(idx, true);

    let widths = draw_bounded(&mut host, idx, 300.0, 6);
    assert!(host.windows[idx].error.is_none(), "{:?}", host.windows[idx].error);
    for (i, w) in widths.iter().enumerate() {
        assert!(
            *w <= 300.5,
            "frame {i} laid out {w} wide inside 300 — a measurement from the other \
             row was trusted: {widths:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&proj);
}

/// The exact right-alignment: laid out from the right edge, so a row whose
/// trailing content changes width is never a frame behind and never overflows.
#[test]
fn right_aligned_content_sits_at_the_edge_on_the_very_first_frame() {
    let proj = temp("guiright");
    install(
        &proj,
        "com.t.right",
        "",
        r#"
        local frame = 0
        ed.window("P", function()
            frame = frame + 1
            gui.horizontal(function()
                gui.small("left")
                gui.rightAligned(function()
                    -- Changing width every frame, which is the case
                    -- flexibleSpace can only be right about one frame later.
                    gui.small(frame % 2 == 0 and "12345678901234567890" or "1")
                end)
            end)
        end)
        "#,
    );
    let mut host = host_for(&proj);
    let idx = host.window_index(host.windows[0].id).unwrap();
    host.set_window_open(idx, true);

    let widths = draw_bounded(&mut host, idx, 300.0, 4);
    assert!(host.windows[idx].error.is_none(), "{:?}", host.windows[idx].error);
    for (i, w) in widths.iter().enumerate() {
        assert!(*w <= 300.5, "frame {i} laid out {w} wide inside 300: {widths:?}");
    }
    // It FILLS the row from the first frame — which is the difference from the
    // measured spacer, whose first frame claims nothing.
    assert!(
        widths[0] >= 299.0,
        "the first frame should already reach the right edge: {widths:?}"
    );
    let _ = std::fs::remove_dir_all(&proj);
}

/// **A package that paints has to be able to measure.**
///
/// Without `gui.measure` the only way to place a second piece of text after a
/// first is characters × an assumed width. That is wrong for every proportional
/// face by a little and for an `i` beside a `W` by a lot, so hand-laid-out text
/// drifts — emphasis lands past the word it belongs to and the right edge goes
/// ragged, which reads as a layout bug rather than as a missing measurement.
#[test]
fn painted_text_can_be_measured_before_it_is_drawn() {
    let proj = temp("guimeasure");
    install(
        &proj,
        "com.t.m",
        "",
        r#"
        ed.window("P", function()
            local narrow = gui.measure("i")
            local wide   = gui.measure("W")
            local one    = gui.measure("hello")
            local twice  = gui.measure("hellohello")
            local big    = gui.measure("hello", 26)
            ed.log(tostring(narrow.w > 0), tostring(narrow.h > 0),
                   tostring(wide.w > narrow.w),
                   tostring(twice.w > one.w),
                   tostring(big.w > one.w),
                   tostring(gui.measure("").w == 0))
        end)
        "#,
    );
    let mut host = host_for(&proj);
    let idx = host.window_index(host.windows[0].id).unwrap();
    host.set_window_open(idx, true);
    host.take_log();
    draw_once(&mut host, idx);
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();

    // Six trues: a measurement has a width and a height, `W` is wider than `i`,
    // twice the text is wider than the text, a bigger size is wider again, and
    // nothing measures as nothing.
    //
    // The `W` versus `i` one is the whole point — a proportional face is not a
    // grid, and an assumed character width cannot tell them apart. That
    // difference IS the drift this call exists to remove.
    assert_eq!(
        log,
        vec!["true\ttrue\ttrue\ttrue\ttrue\ttrue".to_string()],
        "{log:?}"
    );
    assert!(host.windows[0].error.is_none(), "{:?}", host.windows[0].error);
    let _ = std::fs::remove_dir_all(&proj);
}

#[test]
fn a_panel_draws_widgets_and_gets_their_values_back() {
    let proj = temp("guidraw");
    install(
        &proj,
        "com.t.a",
        "",
        r#"
        ed.window("P", function()
            gui.heading("Title")
            gui.label("a label", "and its hover text")
            local clicked = gui.button("Go")
            local n = gui.slider(3, 0, 10, "n")
            local s = gui.textField("hello", "hint")
            local on = gui.checkbox(true, "on")
            local pick = gui.combo("mode", {"a", "b", "c"}, 2)
            gui.horizontal(function()
                gui.label("inside")
                gui.separator()
            end)
            gui.group(function() gui.small("grouped") end)
            gui.rectFilled(0, 0, 10, 10, 1, 0, 0)
            gui.reserve(20, 20)
            local m = gui.mouse()
            ed.log(tostring(clicked), tostring(n), s, tostring(on), tostring(pick),
                   tostring(m.inside))
        end)
        "#,
    );
    let mut host = host_for(&proj);
    let idx = host.window_index(host.windows[0].id).unwrap();
    host.set_window_open(idx, true);
    host.take_log();
    draw_once(&mut host, idx);
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    // Untouched widgets hand their value straight back — which is what makes
    // `x = gui.slider(x, …)` the whole of the state management.
    assert_eq!(log, vec!["false\t3\thello\ttrue\t2\tfalse".to_string()], "{log:?}");
    assert!(host.windows[0].error.is_none(), "{:?}", host.windows[0].error);
    let _ = std::fs::remove_dir_all(&proj);
}

/// `gui.poly` fills a convex shape, and says so when the coordinates cannot
/// pair up.
///
/// The odd-length case is the one worth a test: a flat coordinate list is easy
/// to build one element short, and egui would happily fill whatever the pairs
/// came out as. A polygon quietly missing its last vertex looks like the data
/// was wrong rather than the call.
#[test]
fn gui_poly_fills_and_rejects_an_odd_list() {
    let proj = temp("guipoly");
    install(
        &proj,
        "com.t.a",
        "",
        r#"
        ed.window("Fill", function()
            gui.poly({0, 0, 40, 0, 40, 30, 0, 30}, 0.2, 0.6, 0.9, 0.5)
            gui.poly({0, 0, 10, 10}, 1, 1, 1)   -- two points: nothing, not an error
            gui.reserve(40, 30)
        end)
        ed.window("Odd", function()
            gui.poly({0, 0, 40, 0, 40}, 1, 0, 0)
        end)
        "#,
    );
    let mut host = host_for(&proj);

    let good = host.window_index(host.windows[0].id).unwrap();
    host.set_window_open(good, true);
    draw_once(&mut host, good);
    assert!(host.windows[good].error.is_none(), "{:?}", host.windows[good].error);

    let odd = host.window_index(host.windows[1].id).unwrap();
    host.set_window_open(odd, true);
    draw_once(&mut host, odd);
    let err = host.windows[odd].error.clone().expect("an odd coordinate list must be refused");
    assert!(err.contains("even number"), "{err}");

    let _ = std::fs::remove_dir_all(&proj);
}

/// The rule that makes the scoped binding worth its complexity: a panel cannot
/// keep a widget function and call it next frame.
#[test]
fn a_stashed_gui_function_refuses_rather_than_drawing_into_a_dead_layout() {
    let proj = temp("guistash");
    install(
        &proj,
        "com.t.a",
        "",
        r#"
        STASHED = nil
        ed.window("P", function()
            STASHED = STASHED or gui.label
            gui.label("drawn")
        end)
        ed.onUpdate(function()
            if STASHED then
                local ok, err = pcall(function() STASHED("too late") end)
                ed.log(tostring(ok), tostring(err))
            end
        end)
        "#,
    );
    let mut host = host_for(&proj);
    let idx = host.window_index(host.windows[0].id).unwrap();
    host.set_window_open(idx, true);
    draw_once(&mut host, idx);
    host.take_log();
    host.fire(HookKind::Update);
    let log = host.take_log();
    assert!(log[0].msg.starts_with("false"), "{}", log[0].msg);
    let _ = std::fs::remove_dir_all(&proj);
}

/// A `gui.*` call that raises inside a nested layout must not leave the layout
/// stack pointing at a `Ui` that has ended.
#[test]
fn an_error_inside_a_nested_layout_is_reported_and_the_panel_recovers() {
    let proj = temp("guinest");
    install(
        &proj,
        "com.t.a",
        "",
        r#"
        BOOM = true
        ed.window("P", function()
            gui.horizontal(function()
                if BOOM then error("inside") end
            end)
        end)
        "#,
    );
    let mut host = host_for(&proj);
    let idx = host.window_index(host.windows[0].id).unwrap();
    host.set_window_open(idx, true);
    draw_once(&mut host, idx);
    assert!(host.windows[0].error.is_some());
    // …and the next frame draws the error rather than raising again.
    host.take_log();
    draw_once(&mut host, idx);
    assert!(host.take_log().is_empty());
    let _ = std::fs::remove_dir_all(&proj);
}

/// Every name an extension can reach must appear in the reference. An
/// undocumented binding is a build failure here, the same way it is for the
/// game's Lua API — the API somebody cannot find is the one that gets
/// reinvented badly.
#[test]
fn every_name_in_the_environment_is_in_the_reference() {
    let proj = temp("apidocs");
    install(
        &proj,
        "com.t.a",
        "Network, Browser, Files",
        r#"
        local names = {}
        local function walk(prefix, t)
            for k, v in pairs(t) do
                if type(k) == "string" then names[#names + 1] = prefix .. k end
            end
        end
        walk("ed.", ed)
        walk("ed.package.", ed.package)
        walk("scene.", scene)
        walk("selection.", selection)
        walk("handles.", handles)
        walk("nav.", nav)
        walk("mesh.", mesh)
        walk("http.", http)
        walk("sys.", sys)
        walk("ed.prefs.", ed.prefs)
        table.sort(names)
        ed.log(table.concat(names, " "))
        "#,
    );
    let host = host_for(&proj);
    // The walk above is written by hand, so it can quietly fall behind the
    // environment it is meant to cover — a whole new table would simply not be
    // visited and every name in it would pass undocumented. This builds the
    // real environment and insists that the tables in it are the tables walked.
    {
        let lua = mlua::Lua::new();
        let shared = Rc::new(Shared::default());
        let state = PkgState {
            id: "com.t.a".into(),
            name: "t".into(),
            version: "1.0.0".into(),
            root: proj.clone(),
            permissions: vec![Permission::Network, Permission::Browser, Permission::Files],
            failed: None,
        };
        let env = super::api::build_env(&lua, &shared, 0, &state, None).unwrap();
        let mut tables: Vec<String> = env
            .pairs::<String, mlua::Value>()
            .flatten()
            .filter(|(_, v)| v.is_table())
            .map(|(k, _)| k)
            .collect();
        // Lua's own, which the reference does not document because they are
        // Lua's, and the two `os` carries.
        tables.retain(|k| {
            !matches!(k.as_str(), "string" | "table" | "math" | "coroutine" | "bit" | "os" | "json")
        });
        tables.sort();
        assert_eq!(
            tables,
            ["ed", "handles", "http", "mesh", "nav", "scene", "selection", "sys"],
            "the environment has a table the coverage walk above does not visit"
        );
    }
    let log = host.take_log();
    assert!(!log.is_empty(), "the package did not run");
    let doc = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/editor-scripting.md"),
    )
    .expect("docs/editor-scripting.md");

    // The hooks are documented as a table of `ed.onX(fn)` rows and the stores as
    // one shared row, so the bare member name is what has to appear.
    let doc = documented_code(&doc);
    let mut missing: Vec<String> = Vec::new();
    for full in log[0].msg.split_whitespace() {
        let short = full.rsplit('.').next().unwrap_or(full);
        if !doc.contains(full) && !doc.contains(short) {
            missing.push(full.to_string());
        }
    }
    assert!(
        missing.is_empty(),
        "{} name(s) an extension can reach are missing from docs/editor-scripting.md: {}",
        missing.len(),
        missing.join(", ")
    );
    let _ = std::fs::remove_dir_all(&proj);
}

/// The reference's **code** — everything inside backticks, fenced blocks
/// included — and nothing else.
///
/// The coverage tests below used to search the whole file, which passes any
/// binding whose name is also an ordinary English word: `measure` is
/// "documented" by the sentence explaining why measurement matters, and `docs`
/// by every mention of `docs/scripting.md`. A guard that a word in the prose
/// satisfies is not a guard, and this is meant to be a build failure.
#[cfg(test)]
fn documented_code(doc: &str) -> String {
    let mut out = String::new();
    let mut rest = doc;
    while let Some(open) = rest.find('`') {
        rest = &rest[open + 1..];
        // A fence is three backticks; its content runs to the closing fence.
        let (fence, body) = if let Some(after) = rest.strip_prefix("``") {
            ("```", after)
        } else {
            ("`", rest)
        };
        match body.find(fence) {
            Some(end) => {
                out.push_str(&body[..end]);
                out.push('\n');
                rest = &body[end + fence.len()..];
            }
            None => break,
        }
    }
    out
}

/// …and the same for `gui`, which only exists while drawing.
#[test]
fn every_gui_widget_is_in_the_reference() {
    let proj = temp("guidocs");
    install(
        &proj,
        "com.t.a",
        "",
        r#"
        ed.window("P", function()
            local names = {}
            for k in pairs(gui) do names[#names + 1] = k end
            table.sort(names)
            ed.log(table.concat(names, " "))
        end)
        "#,
    );
    let mut host = host_for(&proj);
    let idx = host.window_index(host.windows[0].id).unwrap();
    host.set_window_open(idx, true);
    host.take_log();
    draw_once(&mut host, idx);
    let log = host.take_log();
    assert!(!log.is_empty(), "the panel did not draw");
    let doc = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/editor-scripting.md"),
    )
    .unwrap();
    let doc = documented_code(&doc);
    let missing: Vec<&str> =
        log[0].msg.split_whitespace().filter(|n| !doc.contains(*n)).collect();
    assert!(
        missing.is_empty(),
        "gui widget(s) missing from docs/editor-scripting.md: {}",
        missing.join(", ")
    );
    let _ = std::fs::remove_dir_all(&proj);
}

/// One real package, driven end to end — the only test that proves a folder on
/// disk becomes a working extension with every hook wired up.
///
/// The fixture is a complete extension rather than a stub on purpose: it
/// registers a panel, a Scene-view overlay with world-space handles, two menu
/// items and a shortcut, reads the scene, edits it with undo, and keeps a
/// preference. A stub would still pass `reload` and would prove none of that.
///
/// It used to be a package we shipped; that came off the shelf, and the code
/// stayed here because deleting it would have left the whole extension API
/// with no end-to-end coverage at all.
#[test]
fn the_example_package_loads_draws_and_survives_every_hook() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/example-package");
    assert!(src.join("package.ron").exists(), "{}", src.display());
    let proj = temp("example");
    floptle_package::install::install_from_dir(&proj, &src, false).unwrap();

    let mut host = ExtHost::new();
    host.begin_frame(
        Snapshot { project_root: proj.clone(), ..Snapshot::default() },
        SceneMirror::default(),
    );
    // The example declares `engine: ">=0.55.0"`, so it must load on the build
    // that ships it — this is also the check that the two never drift.
    host.reload(&proj, &crate::Editor::engine_version());
    assert!(
        host.report.errors().next().is_none(),
        "{:?}",
        host.report.errors().map(|p| &p.message).collect::<Vec<_>>()
    );
    assert_eq!(host.packages.len(), 1);
    assert!(host.packages[0].failed.is_none(), "{:?}", host.packages[0].failed);
    assert_eq!(host.windows.len(), 1);
    assert_eq!(host.overlays.len(), 1);
    assert_eq!(host.menus.len(), 2);
    assert_eq!(host.shortcuts.len(), 1);

    // A scene with something in it, so `survey()` has work to do.
    let mut w = floptle_core::World::new();
    for (i, name) in ["Ground", "Crate", "Lamp"].iter().enumerate() {
        let e = w.spawn();
        w.insert(e, floptle_core::Name((*name).into()));
        w.insert(
            e,
            floptle_core::Matter::Primitive {
                shape: floptle_core::Shape::Cube,
                color: [1.0; 3],
            },
        );
        w.insert(
            e,
            floptle_core::Transform::from_translation(floptle_core::math::DVec3::new(
                i as f64 * 3.0,
                0.0,
                0.0,
            )),
        );
    }
    let mirror = SceneMirror::build(&w, &|_, _| Some(6.0), &|_, _| Some([6.0, 6.0, 6.0]));
    let ids: Vec<u32> = mirror.nodes.iter().map(|n| n.id).collect();
    host.begin_frame(
        Snapshot {
            project_root: proj.clone(),
            scene: "scenes/first.ron".into(),
            selection: ids,
            ..Snapshot::default()
        },
        mirror,
    );

    for kind in HookKind::ALL {
        host.fire(*kind);
    }
    // Every node is over the threshold at radius 6, so the overlay markers are
    // there to draw — a silently empty draw would pass a weaker assertion.
    assert!(!host.handles().is_empty());

    let idx = host.window_index(host.windows[0].id).unwrap();
    host.set_window_open(idx, true);
    draw_once(&mut host, idx);
    assert!(host.windows[0].error.is_none(), "{:?}", host.windows[0].error);

    // …and every menu item runs without raising.
    for i in 0..host.menus.len() {
        host.run_menu(i);
    }
    host.run_shortcut(0);
    let errors: Vec<String> = host
        .take_log()
        .into_iter()
        .filter(|l| l.level == ExtLevel::Error)
        .map(|l| l.msg)
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
    let _ = std::fs::remove_dir_all(&proj);
}

#[test]
fn a_package_can_raycast_the_scene() {
    let proj = temp("raycast");
    install(
        &proj,
        "com.t.a",
        "",
        r#"
        ed.onUpdate(function()
            local hit = scene.raycast(vec3(0, 20, 0), vec3(0, -1, 0))
            if hit then
                ed.log(scene.info(hit.node).name, tostring(hit.distance),
                       tostring(hit.normal.y))
            else
                ed.log("nothing")
            end
        end)
        "#,
    );
    let mut host = host_for(&proj);

    let mut w = floptle_core::World::new();
    let e = w.spawn();
    w.insert(e, floptle_core::Name("Ground".into()));
    w.insert(e, floptle_core::Matter::Empty);
    host.begin_frame(
        Snapshot { project_root: proj.clone(), ..Snapshot::default() },
        SceneMirror::build(&w, &|_, _| Some(1.0), &|_, _| Some([10.0, 1.0, 10.0])),
    );
    host.take_log();
    host.fire(HookKind::Update);
    assert_eq!(host.take_log()[0].msg, "Ground\t19\t1");

    // An empty scene answers nothing rather than raising.
    host.begin_frame(
        Snapshot { project_root: proj.clone(), ..Snapshot::default() },
        SceneMirror::default(),
    );
    host.fire(HookKind::Update);
    assert_eq!(host.take_log()[0].msg, "nothing");
    let _ = std::fs::remove_dir_all(&proj);
}

/// A floor with a hole in it, baked — so there is more than one rectangle and
/// a route has somewhere to go round.
fn a_baked_floor() -> floptle_nav::NavMesh {
    let quad = |x0: f32, x1: f32, z0: f32, z1: f32| {
        vec![
            floptle_nav::Tri::new([x0, 0.0, z0], [x1, 0.0, z0], [x0, 0.0, z1]),
            floptle_nav::Tri::new([x1, 0.0, z0], [x1, 0.0, z1], [x0, 0.0, z1]),
        ]
    };
    let mut tris = quad(0.0, 12.0, 0.0, 4.0);
    tris.extend(quad(0.0, 12.0, 8.0, 12.0));
    tris.extend(quad(0.0, 4.0, 4.0, 8.0));
    tris.extend(quad(8.0, 12.0, 4.0, 8.0));
    floptle_nav::bake(&tris, &floptle_nav::NavSettings::default()).expect("this floor bakes")
}

fn host_with_a_navmesh(proj: &Path) -> ExtHost {
    let mut host = ExtHost::new();
    host.begin_frame(
        Snapshot { project_root: proj.to_path_buf(), ..Snapshot::default() },
        SceneMirror::default(),
    );
    // Before the reload, because a package reads the level while it loads.
    host.set_nav_mesh(Some(a_baked_floor()));
    host.reload(proj, &engine());
    host
}

/// An extension can read the level's navmesh — the shape of the floor, what
/// kind of ground each piece is, and where the nearest standable point is.
#[test]
fn a_package_reads_the_baked_navmesh() {
    let proj = temp("nav-read");
    install(
        &proj,
        "com.t.nav",
        "",
        r#"
        local a, n = nav.areas()
        ed.log("ready " .. tostring(nav.ready()))
        ed.log("rects " .. n .. " numbers " .. #a)
        ed.log("ground " .. nav.ground()[1].name)
        ed.log("stride " .. nav.AREA_STRIDE)
        "#,
    );
    let host = host_with_a_navmesh(&proj);
    assert!(host.packages[0].failed.is_none(), "{:?}", host.packages[0].failed);
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert!(log.contains(&"ready true".to_string()), "{log:?}");
    assert!(log.contains(&"ground walkable".to_string()), "{log:?}");
    assert!(log.contains(&"stride 11".to_string()), "{log:?}");
    // Four rectangles at least — a floor with a hole cannot be one — and the
    // flat array is exactly stride-many numbers per rectangle.
    let rects = log.iter().find(|l| l.starts_with("rects ")).expect("{log:?}");
    let parts: Vec<&str> = rects.split_whitespace().collect();
    let (n, numbers): (usize, usize) = (parts[1].parse().unwrap(), parts[3].parse().unwrap());
    assert!(n >= 4, "a floor with a hole in it baked into {n} rectangles");
    assert_eq!(numbers, n * 11);
    let _ = std::fs::remove_dir_all(&proj);
}

/// **The first thing anybody does with a navmesh is draw it.** `nav.*` answers
/// in the scripting runtime's vector, which is userdata and not a table, and
/// `handles.*` reads tables — so without the two agreeing, the obvious two-line
/// package raises on its second line.
#[test]
fn a_point_from_nav_can_be_drawn_with_handles() {
    let proj = temp("nav-draw");
    install(
        &proj,
        "com.t.nav",
        "",
        r#"
        local p = nav.nearest(vec3(1, 0, 1))
        ed.log("nearest is " .. type(p))
        handles.dot(p, 4)
        handles.line(p, nav.nearest(vec3(11, 0, 11)))
        "#,
    );
    let host = host_with_a_navmesh(&proj);
    assert!(host.packages[0].failed.is_none(), "{:?}", host.packages[0].failed);
    assert_eq!(host.shared.handles.borrow().len(), 2, "the drawing did not reach the queue");
    let _ = std::fs::remove_dir_all(&proj);
}

/// The editor gets the reading half and nothing that moves: there is no
/// simulation for an agent to walk in, and an obstacle carved into the editor's
/// own bake would be a level edit made by a panel.
#[test]
fn an_extension_cannot_drive_or_carve() {
    let proj = temp("nav-half");
    install(
        &proj,
        "com.t.nav",
        "",
        r#"
        ed.log("agent " .. type(nav.agent))
        ed.log("obstacle " .. type(nav.obstacle))
        ed.log("link " .. type(nav.link))
        ed.log("areas " .. type(nav.areas))
        "#,
    );
    let host = host_with_a_navmesh(&proj);
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert!(log.contains(&"agent nil".to_string()), "{log:?}");
    assert!(log.contains(&"obstacle nil".to_string()), "{log:?}");
    assert!(log.contains(&"link nil".to_string()), "{log:?}");
    assert!(log.contains(&"areas function".to_string()), "{log:?}");
    let _ = std::fs::remove_dir_all(&proj);
}

/// A project nobody has baked is the ordinary state of a new one, and a package
/// that runs on every scene has to survive it.
#[test]
fn nav_answers_nothing_rather_than_raising_with_no_bake() {
    let proj = temp("nav-none");
    install(
        &proj,
        "com.t.nav",
        "",
        r#"
        local a, n = nav.areas()
        ed.log("ready " .. tostring(nav.ready()) .. " areas " .. type(a) .. " n " .. n)
        ed.log("ground " .. type(nav.ground()) .. " links " .. type(nav.offLinks()))
        "#,
    );
    let host = host_for(&proj);
    assert!(host.packages[0].failed.is_none(), "{:?}", host.packages[0].failed);
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert!(log.contains(&"ready false areas nil n 0".to_string()), "{log:?}");
    assert!(log.contains(&"ground nil links nil".to_string()), "{log:?}");
    let _ = std::fs::remove_dir_all(&proj);
}

/// A timer fires once, on the editor's clock, and not before its time.
#[test]
fn a_one_shot_timer_fires_once_when_it_is_due() {
    let proj = temp("timer-once");
    install(
        &proj,
        "com.t.timer",
        "",
        r#"
        fired = 0
        ed.after(1.0, function() fired = fired + 1 ed.log("fired " .. fired) end)
        ed.onUpdate(function() end)
        "#,
    );
    let mut host = host_for(&proj);
    let at = |host: &mut ExtHost, t: f64| {
        host.begin_frame(
            Snapshot { project_root: proj.clone(), time: t, ..Snapshot::default() },
            SceneMirror::default(),
        );
        host.tick_timers();
    };
    at(&mut host, 0.5);
    assert!(host.take_log().is_empty(), "it fired before it was due");
    at(&mut host, 1.5);
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert_eq!(log, vec!["fired 1".to_string()]);
    // …and a one-shot is gone afterwards rather than firing every frame after
    // its deadline, which is the way this goes wrong.
    at(&mut host, 9.0);
    assert!(host.take_log().is_empty(), "a one-shot fired twice");
    assert!(host.timers.is_empty(), "a spent timer was left on the list");
    let _ = std::fs::remove_dir_all(&proj);
}

/// A repeat keeps its period rather than drifting by a frame each time, and a
/// long stall does not make it fire once for every period it missed.
#[test]
fn a_repeating_timer_keeps_its_period_and_does_not_catch_up() {
    let proj = temp("timer-every");
    install(
        &proj,
        "com.t.timer",
        "",
        r#"
        n = 0
        ed.every(0.5, function() n = n + 1 ed.log("tick " .. n) end)
        "#,
    );
    let mut host = host_for(&proj);
    let at = |host: &mut ExtHost, t: f64| {
        host.begin_frame(
            Snapshot { project_root: proj.clone(), time: t, ..Snapshot::default() },
            SceneMirror::default(),
        );
        host.tick_timers();
        host.take_log().len()
    };
    assert_eq!(at(&mut host, 0.4), 0);
    assert_eq!(at(&mut host, 0.6), 1);
    assert_eq!(at(&mut host, 1.1), 1);
    // A minute of nothing — a modal dialog, a bake, a laptop lid. One firing,
    // not a hundred and twenty.
    assert_eq!(at(&mut host, 61.0), 1, "it tried to catch up on a stall");
    assert_eq!(at(&mut host, 61.6), 1, "and its period survived the stall");
    let _ = std::fs::remove_dir_all(&proj);
}

/// Cancelling from inside the callback is the case that eats its own list.
#[test]
fn a_timer_can_cancel_itself_from_inside_its_own_callback() {
    let proj = temp("timer-cancel");
    install(
        &proj,
        "com.t.timer",
        "",
        r#"
        n = 0
        local t
        t = ed.every(0.5, function()
            n = n + 1
            ed.log("tick " .. n)
            if n == 2 then t:cancel() end
        end)
        ed.every(0.5, function() ed.log("neighbour") end)
        "#,
    );
    let mut host = host_for(&proj);
    let at = |host: &mut ExtHost, t: f64| {
        host.begin_frame(
            Snapshot { project_root: proj.clone(), time: t, ..Snapshot::default() },
            SceneMirror::default(),
        );
        host.tick_timers();
        let msgs: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
        msgs
    };
    assert_eq!(at(&mut host, 0.6), vec!["tick 1", "neighbour"]);
    assert_eq!(at(&mut host, 1.1), vec!["tick 2", "neighbour"]);
    // The neighbour must survive: a cancel that shortened the list mid-walk
    // would take whatever moved into the freed slot with it.
    assert_eq!(at(&mut host, 1.6), vec!["neighbour"]);
    assert_eq!(host.timers.len(), 1);
    let _ = std::fs::remove_dir_all(&proj);
}

/// A package that never declared anything still gets real randomness. It is not
/// a capability — it is the difference between a correct sign-in challenge and a
/// guessable one.
#[test]
fn random_bytes_are_the_length_asked_for_and_not_the_same_twice() {
    let proj = temp("random");
    install(
        &proj,
        "com.t.rand",
        "",
        r#"
        local a, b = ed.randomBytes(32), ed.randomBytes(32)
        ed.log("len " .. #a .. " " .. #b)
        ed.log("same " .. tostring(a == b))
        local ok, err = pcall(ed.randomBytes, 0)
        ed.log("zero " .. tostring(ok))
        "#,
    );
    let host = host_for(&proj);
    assert!(host.packages[0].failed.is_none(), "{:?}", host.packages[0].failed);
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert!(log.contains(&"len 32 32".to_string()), "{log:?}");
    assert!(log.contains(&"same false".to_string()), "{log:?}");
    assert!(log.contains(&"zero false".to_string()), "{log:?}");
    let _ = std::fs::remove_dir_all(&proj);
}

// ---------------------------------------------------------------------------
// Fonts (`floptle/0139`). A package ships a typeface and draws a run of widgets
// in it. The interesting cases are all failure cases: a face that is not there,
// a path that tries to leave the folder, and a reload that must not accumulate.
// ---------------------------------------------------------------------------

/// `install`, plus a `fonts:` list and the files it names.
fn install_with_fonts(proj: &Path, id: &str, faces: &[(&str, &str)], lua: &str) {
    install(proj, id, "", lua);
    let root = proj.join("packages").join(id);
    std::fs::create_dir_all(root.join("fonts")).unwrap();
    let declared: Vec<String> = faces
        .iter()
        .map(|(name, path)| format!("(name: {name:?}, path: {path:?})"))
        .collect();
    std::fs::write(
        root.join("package.ron"),
        format!(
            r#"( id: "{id}", name: "{id}", version: "1.0.0", fonts: [{}] )"#,
            declared.join(", ")
        ),
    )
    .unwrap();
    // A real font, so the loader's "is this a font" check passes on the ones
    // meant to succeed. Whatever egui ships — the point is that it parses.
    let defs = egui::FontDefinitions::default();
    let first = defs.families[&egui::FontFamily::Proportional][0].clone();
    let bytes = defs.font_data[&first].font.to_vec();
    for (_, path) in faces {
        let p = root.join(path);
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        if !p.exists() {
            std::fs::write(&p, &bytes).unwrap();
        }
    }
}

#[test]
fn a_package_ships_a_face_and_draws_a_run_of_widgets_in_it() {
    let proj = temp("fontdraw");
    install_with_fonts(
        &proj,
        "com.t.brand",
        &[("Heading", "fonts/display.ttf")],
        r#"
        ed.window("P", function()
            ed.log("has " .. tostring(gui.hasFont("Heading")))
            ed.log("missing " .. tostring(gui.hasFont("Nope")))
            gui.font("Heading", function() gui.heading("Branded") end)
        end)
        "#,
    );
    let mut host = host_for(&proj);
    assert!(host.packages[0].failed.is_none(), "{:?}", host.packages[0].failed);
    assert_eq!(host.fonts.len(), 1, "the declared face should have been read");
    assert_eq!(host.fonts[0].family, "com.t.brand:Heading");
    assert!(host.fonts_dirty, "the editor has to be told to rebuild the atlas");
    let _ = host.take_log();
    draw_once(&mut host, 0);
    assert!(host.packages[0].failed.is_none(), "{:?}", host.packages[0].failed);
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert!(log.contains(&"has true".to_string()), "{log:?}");
    assert!(log.contains(&"missing false".to_string()), "{log:?}");
    let _ = std::fs::remove_dir_all(&proj);
}

/// The failure that would otherwise be a tofu row or sixty Console lines a
/// second: a panel drawing every frame in a face that is not there.
#[test]
fn a_face_that_is_not_there_draws_in_the_editors_type_and_says_so_once() {
    let proj = temp("fontmissing");
    install(
        &proj,
        "com.t.b",
        "",
        r#"ed.window("P", function() gui.font("Ghost", function() gui.label("x") end) end)"#,
    );
    let mut host = host_for(&proj);
    let _ = host.take_log();
    for _ in 0..5 {
        draw_once(&mut host, 0);
    }
    assert!(
        host.packages[0].failed.is_none(),
        "a missing face is a warning, not a broken panel: {:?}",
        host.packages[0].failed
    );
    let warns: Vec<String> = host
        .take_log()
        .into_iter()
        .filter(|l| l.level == ExtLevel::Warn)
        .map(|l| l.msg)
        .collect();
    assert_eq!(warns.len(), 1, "five frames, one complaint: {warns:?}");
    assert!(warns[0].contains("Ghost"), "{warns:?}");
    let _ = std::fs::remove_dir_all(&proj);
}

/// Two packages both calling their face `"Heading"` must each get their own.
#[test]
fn one_packages_face_is_not_reachable_by_another_packages_name() {
    let proj = temp("fontscope");
    install_with_fonts(&proj, "com.t.one", &[("Heading", "fonts/a.ttf")], r#"
        ed.window("One", function() ed.log("one " .. tostring(gui.hasFont("Heading"))) end)
    "#);
    install(&proj, "com.t.two", "", r#"
        ed.window("Two", function() ed.log("two " .. tostring(gui.hasFont("Heading"))) end)
    "#);
    let mut host = host_for(&proj);
    assert_eq!(host.fonts.len(), 1, "only one package shipped a face");
    let _ = host.take_log();
    draw_once(&mut host, 0);
    draw_once(&mut host, 1);
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert!(log.contains(&"one true".to_string()), "{log:?}");
    assert!(
        log.contains(&"two false".to_string()),
        "com.t.two never shipped a Heading and must not inherit one: {log:?}"
    );
    let _ = std::fs::remove_dir_all(&proj);
}

/// ⟲ Reload all replaces the set. Accumulating would mean the face of a package
/// somebody has just switched off is still registered — and the atlas grows
/// every time somebody presses the button.
#[test]
fn a_reload_replaces_the_font_set_rather_than_adding_to_it() {
    let proj = temp("fontreload");
    install_with_fonts(&proj, "com.t.brand", &[("Heading", "fonts/display.ttf")], r#"
        ed.window("P", function() end)
    "#);
    let mut host = host_for(&proj);
    assert_eq!(host.fonts.len(), 1);
    host.reload(&proj, &engine());
    assert_eq!(host.fonts.len(), 1, "one package, one face, however many reloads");

    floptle_package::install::set_enabled(&proj, "com.t.brand", false).unwrap();
    host.reload(&proj, &engine());
    assert!(host.fonts.is_empty(), "a switched-off package's face must go with it");
    assert!(host.fonts_dirty, "and the atlas has to be rebuilt without it");
    let _ = std::fs::remove_dir_all(&proj);
}

/// A project whose packages ship no fonts must not pay for this feature: no
/// atlas rebuild, on load or on reload.
#[test]
fn a_project_with_no_package_fonts_never_rebuilds_the_atlas() {
    let proj = temp("fontfree");
    install(&proj, "com.t.a", "", r#"ed.window("P", function() end)"#);
    let mut host = host_for(&proj);
    assert!(host.fonts.is_empty());
    assert!(!host.fonts_dirty, "nothing to register, so nothing to rebuild");
    host.reload(&proj, &engine());
    assert!(!host.fonts_dirty);
    let _ = std::fs::remove_dir_all(&proj);
}

/// The one-frame gap: a package has loaded and declared a face, but `set_fonts`
/// has not run yet. epaint **panics** on a `FontFamily::Name` it does not hold,
/// so this must draw in the editor's type rather than take the editor down.
#[test]
fn a_declared_face_egui_has_not_been_given_yet_draws_instead_of_panicking() {
    let proj = temp("fontgap");
    install_with_fonts(
        &proj,
        "com.t.brand",
        &[("Heading", "fonts/display.ttf")],
        r#"ed.window("P", function() gui.font("Heading", function() gui.heading("H") end) end)"#,
    );
    let mut host = host_for(&proj);
    assert_eq!(host.fonts.len(), 1, "declared and read");
    let _ = host.take_log();
    // Deliberately NOT `draw_once`: a bare context, exactly as the editor's is
    // between the package load and the `set_fonts` that follows it.
    let ctx = egui::Context::default();
    let _ = ctx.run_ui(egui::RawInput::default(), |ui| host.draw_window(0, ui));
    assert!(host.packages[0].failed.is_none(), "{:?}", host.packages[0].failed);
    let warns: Vec<String> = host
        .take_log()
        .into_iter()
        .filter(|l| l.level == ExtLevel::Warn)
        .map(|l| l.msg)
        .collect();
    assert!(
        warns.is_empty(),
        "the face IS declared — a frame of the editor's type is not worth a complaint: {warns:?}"
    );
    let _ = std::fs::remove_dir_all(&proj);
}

/// The node document, from Lua's side (`floptle/0142`). What a level-design tool
/// does after it has finished analysing: put something in the level.
#[test]
fn a_package_writes_a_node_document_and_builds_a_subtree() {
    let proj = temp("nodedoc");
    install(
        &proj,
        "com.t.build",
        "",
        r#"
        ed.onUpdate(function()
            for _, id in ipairs(scene.all()) do
                if scene.info(id).name == "Crate" then
                    scene.set(id, { tags = {"cover"}, layer = "props" })
                    scene.setParent(id, nil)
                end
            end
            scene.add({
                name = "Guard Post",
                children = { { name = "Lamp" } },
            })
        end)
        "#,
    );
    let mut host = host_for(&proj);
    let mut w = floptle_core::World::new();
    let e = w.spawn();
    w.insert(e, floptle_core::Name("Crate".into()));
    w.insert(e, floptle_core::Matter::Empty);
    host.begin_frame(
        Snapshot { project_root: proj.clone(), ..Snapshot::default() },
        SceneMirror::build(&w, &|_, _| None, &|_, _| None),
    );
    host.fire(HookKind::Update);

    let cmds = host.take_cmds();
    assert!(
        cmds.iter().any(|c| matches!(c, ExtCmd::NodeSet { id, patch }
            if *id == e.index() && patch["layer"] == "props")),
        "{cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| matches!(c, ExtCmd::NodeSetParent { id, parent: None }
            if *id == e.index())),
        "{cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| matches!(c, ExtCmd::NodeAdd { spec, parent: None }
            if spec["name"] == "Guard Post" && spec["children"][0]["name"] == "Lamp")),
        "{cmds:?}"
    );
    let _ = std::fs::remove_dir_all(&proj);
}

/// A queued edit must not hold anything of Lua's. LuaJIT has ~8000 registry
/// slots and `create_table` PANICS when they run out, so a tool that queues a
/// few hundred edits in a loop would take the editor down if the command carried
/// the table rather than a copy of it.
#[test]
fn queueing_many_document_edits_does_not_run_lua_out_of_registry_slots() {
    let proj = temp("nodedocmany");
    install(
        &proj,
        "com.t.many",
        "",
        r#"
        ed.onUpdate(function()
            for i = 1, 2000 do
                scene.add({ name = "N" .. i, tags = {"generated"} })
            end
        end)
        "#,
    );
    let mut host = host_for(&proj);
    host.fire(HookKind::Update);
    assert!(host.packages[0].failed.is_none(), "{:?}", host.packages[0].failed);
    assert_eq!(host.take_cmds().len(), 2000);
    let _ = std::fs::remove_dir_all(&proj);
}

/// `scene.doc` round-trips: what comes out of a node goes back into one.
///
/// Reading a document and writing it to a new node is the whole basis of
/// "place another one of these", and a field that survives the read but not the
/// write is a copy that quietly is not one.
#[test]
fn a_document_read_from_a_node_can_be_written_to_a_new_one() {
    let proj = temp("docroundtrip");
    install(
        &proj,
        "com.t.copy",
        "",
        r#"
        ed.onUpdate(function()
            local id = selection.active()
            if not id then return end
            local doc = scene.doc(id)
            ed.log("name " .. tostring(doc.name))
            ed.log("tags " .. tostring(doc.tags and doc.tags[1]))
            doc.name = doc.name .. " copy"
            scene.add(doc)
        end)
        "#,
    );
    let mut host = host_for(&proj);
    let mut w = floptle_core::World::new();
    let e = w.spawn();
    w.insert(e, floptle_core::Name("Crate".into()));
    w.insert(e, floptle_core::Matter::Empty);
    w.insert(e, floptle_core::Tags(vec!["cover".into()]));

    let mut mirror = SceneMirror::build(&w, &|_, _| None, &|_, _| None);
    // What `Editor::fill_mirror_docs` does for the selection.
    let doc = floptle_scene::NodeDoc {
        name: "Crate".into(),
        tags: vec!["cover".into()],
        ..blank_doc()
    };
    mirror.docs.insert(e.index(), serde_json::to_value(&doc).unwrap());

    host.begin_frame(
        Snapshot {
            project_root: proj.clone(),
            selection: vec![e.index()],
            ..Snapshot::default()
        },
        mirror,
    );
    host.fire(HookKind::Update);
    assert!(host.packages[0].failed.is_none(), "{:?}", host.packages[0].failed);

    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert!(log.contains(&"name Crate".to_string()), "{log:?}");
    assert!(log.contains(&"tags cover".to_string()), "{log:?}");

    let cmds = host.take_cmds();
    assert!(
        cmds.iter().any(|c| matches!(c, ExtCmd::NodeAdd { spec, .. }
            if spec["name"] == "Crate copy" && spec["tags"][0] == "cover")),
        "the copy should carry the original's tags: {cmds:?}"
    );
    let _ = std::fs::remove_dir_all(&proj);
}

/// Reading a node that is not selected RAISES. A nil would have a tool place an
/// empty node and report success, which is the failure this API exists to avoid.
#[test]
fn reading_the_document_of_an_unselected_node_says_why_rather_than_answering_nil() {
    let proj = temp("docunselected");
    install(
        &proj,
        "com.t.copy",
        "",
        r#"
        ed.onUpdate(function()
            local ok, err = pcall(scene.doc, 1)
            ed.log("ok " .. tostring(ok))
            ed.log("err " .. tostring(err))
        end)
        "#,
    );
    let mut host = host_for(&proj);
    let mut w = floptle_core::World::new();
    let e = w.spawn();
    w.insert(e, floptle_core::Name("Crate".into()));
    w.insert(e, floptle_core::Matter::Empty);
    host.begin_frame(
        Snapshot { project_root: proj.clone(), ..Snapshot::default() },
        SceneMirror::build(&w, &|_, _| None, &|_, _| None),
    );
    host.fire(HookKind::Update);
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert!(log.contains(&"ok false".to_string()), "{log:?}");
    assert!(
        log.iter().any(|l| l.contains("selected") || l.contains("no node")),
        "the message has to say what to do about it: {log:?}"
    );
    let _ = std::fs::remove_dir_all(&proj);
}

/// **A tool that acts on a level has to be able to read one.**
///
/// `scene.doc` answers for the selection, which is right for the mirror and
/// wrong as the only way in: renaming across a level, collecting every
/// user-facing string, or proposing a reorganisation all mean reading nodes
/// nobody has selected — and `selection.set` is not a workaround, it is an edit
/// to somebody's selection made in order to perform a read.
#[test]
fn a_package_can_read_the_documents_of_nodes_that_are_not_selected() {
    let proj = temp("docsbatch");
    install(
        &proj,
        "com.t.docs",
        "",
        r#"
        ed.onUpdate(function()
            scene.docs({ 1, 2, 99 }, function(docs, missing)
                local names = {}
                for id, d in pairs(docs) do names[#names + 1] = id .. "=" .. d.name end
                table.sort(names)
                ed.log("docs " .. table.concat(names, ","))
                ed.log("missing " .. table.concat(missing, ","))
            end)
        end)
        "#,
    );
    let mut host = host_for(&proj);
    // Nothing selected, deliberately: the whole point is that this does not
    // need one.
    host.begin_frame(
        Snapshot { project_root: proj.clone(), ..Snapshot::default() },
        SceneMirror::default(),
    );
    host.fire(HookKind::Update);

    let reqs = host.take_doc_requests();
    assert_eq!(reqs.len(), 1, "the read should have queued");
    assert_eq!(reqs[0].ids, vec![1, 2, 99], "sorted, deduped, in one batch");

    // What the editor does, without an editor.
    for req in reqs {
        let mut docs = Vec::new();
        let mut missing = Vec::new();
        for id in &req.ids {
            match id {
                1 => docs.push((
                    1u32,
                    serde_json::to_value(floptle_scene::NodeDoc {
                        name: "Crate".into(),
                        ..blank_doc()
                    })
                    .unwrap(),
                )),
                2 => docs.push((
                    2u32,
                    serde_json::to_value(floptle_scene::NodeDoc {
                        name: "Door".into(),
                        ..blank_doc()
                    })
                    .unwrap(),
                )),
                other => missing.push(*other),
            }
        }
        host.deliver_docs(req.cb, docs, missing);
    }

    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert!(log.contains(&"docs 1=Crate,2=Door".to_string()), "{log:?}");
    // **Reported, not dropped.** A node destroyed between the ask and the
    // answer is ordinary; a batch that quietly returns fewer nodes than it was
    // given is how a tool reports renaming three having renamed two.
    assert!(log.contains(&"missing 99".to_string()), "{log:?}");
    let _ = std::fs::remove_dir_all(&proj);
}

/// Asking for more than one read serves says the number rather than hitching.
#[test]
fn a_document_read_too_big_to_serve_at_once_says_so() {
    let proj = temp("docscap");
    install(
        &proj,
        "com.t.docsbig",
        "",
        r#"
        ed.onUpdate(function()
            local ids = {}
            for i = 1, scene.maxDocs + 1 do ids[i] = i end
            local ok, err = pcall(scene.docs, ids, function() end)
            ed.log("ok " .. tostring(ok))
            ed.log("err " .. tostring(err))
        end)
        "#,
    );
    let mut host = host_for(&proj);
    host.fire(HookKind::Update);
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert!(log.contains(&"ok false".to_string()), "{log:?}");
    assert!(
        log.iter().any(|l| l.contains("batches")),
        "the message has to say what to do about it: {log:?}"
    );
    assert!(host.take_doc_requests().is_empty(), "nothing should have queued");
    let _ = std::fs::remove_dir_all(&proj);
}

/// The smallest `NodeDoc` the tests can build, so a field added to it does not
/// mean editing every test that ever made one.
#[cfg(test)]
fn blank_doc() -> floptle_scene::NodeDoc {
    serde_json::from_value(serde_json::json!({ "name": "x" })).unwrap()
}

/// A UI element is an ordinary node carrying an `ElementSpec`, so its `kind` is
/// `"empty"` — a package had no way to tell a button from a folder, which is
/// the whole basis of any tool that reasons about a screen.
#[test]
fn a_package_can_tell_a_button_from_a_folder() {
    let proj = temp("uikind");
    install(
        &proj,
        "com.t.ui",
        "",
        r#"
        ed.onUpdate(function()
            for _, id in ipairs(scene.all()) do
                local n = scene.info(id)
                if n.ui then
                    ed.log(n.name .. " is a " .. n.ui.element
                           .. " saying " .. n.ui.text
                           .. " interactive=" .. tostring(n.ui.interactive))
                else
                    ed.log(n.name .. " is not ui")
                end
            end
        end)
        "#,
    );
    let mut host = host_for(&proj);
    let mut w = floptle_core::World::new();

    let folder = w.spawn();
    w.insert(folder, floptle_core::Name("Menu".into()));
    w.insert(folder, floptle_core::Matter::Empty);

    let button = w.spawn();
    w.insert(button, floptle_core::Name("Button (3)".into()));
    w.insert(button, floptle_core::Matter::Empty);
    w.insert(
        button,
        floptle_ui::ElementSpec {
            button: true,
            text: Some(floptle_ui::TextSpec {
                text: "Start Game".into(),
                ..Default::default()
            }),
            ..Default::default()
        },
    );

    host.begin_frame(
        Snapshot { project_root: proj.clone(), ..Snapshot::default() },
        SceneMirror::build(&w, &|_, _| None, &|_, _| None),
    );
    host.fire(HookKind::Update);
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert!(
        log.contains(&"Button (3) is a button saying Start Game interactive=true".to_string()),
        "{log:?}"
    );
    assert!(log.contains(&"Menu is not ui".to_string()), "{log:?}");
    let _ = std::fs::remove_dir_all(&proj);
}

/// `mesh.read` end to end from Lua: a request queues, and the answer is flat
/// arrays a package can walk.
#[test]
fn a_package_reads_a_nodes_triangles() {
    let proj = temp("meshread");
    install(
        &proj,
        "com.t.mesh",
        "",
        r#"
        ed.onUpdate(function()
            mesh.read(1, function(m, err)
                if not m then ed.log("err " .. tostring(err)) return end
                ed.log("source " .. m.source)
                ed.log("counts " .. m.vertices .. " " .. m.triangles)
                ed.log("flat " .. #m.positions .. " " .. #m.indices)
                -- Indices are ZERO based and address the flat array; getting
                -- this wrong is the first thing anybody does.
                local i0 = m.indices[1]
                ed.log("first x " .. tostring(m.positions[i0 * 3 + 1] ~= nil))
            end)
        end)
        "#,
    );
    let mut host = host_for(&proj);
    host.fire(HookKind::Update);

    let reqs = host.take_mesh_requests();
    assert_eq!(reqs.len(), 1, "the read should have queued");

    // What the editor does, without an editor.
    let mut w = floptle_core::World::new();
    let e = w.spawn();
    w.insert(e, floptle_core::Name("Box".into()));
    w.insert(
        e,
        floptle_core::Matter::Primitive {
            shape: floptle_core::Shape::Cube,
            color: [1.0; 3],
        },
    );
    let geo =
        crate::mesh_read::read_node(&w, e, std::path::Path::new("."), &Default::default()).unwrap();
    let (verts, tris) = (geo.vertex_count(), geo.triangle_count());
    for req in reqs {
        let g = crate::mesh_read::read_node(&w, e, std::path::Path::new("."), &Default::default());
        host.deliver_mesh(req.cb, g);
    }

    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert!(log.contains(&"source primitive".to_string()), "{log:?}");
    assert!(log.contains(&format!("counts {verts} {tris}")), "{log:?}");
    assert!(log.contains(&format!("flat {} {}", verts * 3, tris * 3)), "{log:?}");
    assert!(log.contains(&"first x true".to_string()), "{log:?}");
    let _ = std::fs::remove_dir_all(&proj);
}

/// A read that cannot be answered calls back with `nil` and a REASON, rather
/// than never calling back — a callback that silently never runs is the worst
/// of the three possible failures.
#[test]
fn a_mesh_read_that_fails_still_calls_back_and_says_why() {
    let proj = temp("meshfail");
    install(
        &proj,
        "com.t.mesh",
        "",
        r#"
        ed.onUpdate(function()
            mesh.read(99, function(m, err)
                ed.log("got " .. tostring(m) .. " / " .. tostring(err))
            end)
        end)
        "#,
    );
    let mut host = host_for(&proj);
    host.fire(HookKind::Update);
    for req in host.take_mesh_requests() {
        host.deliver_mesh(req.cb, Err("no node 99".into()));
    }
    let log: Vec<String> = host.take_log().into_iter().map(|l| l.msg).collect();
    assert!(log.contains(&"got nil / no node 99".to_string()), "{log:?}");
    let _ = std::fs::remove_dir_all(&proj);
}

/// `ed.lookAt` — a tool with a list of places has to be able to take you to one.
///
/// The distance is checked because the failure is silent: a camera glided to
/// zero metres from a point lands inside the geometry and reads as the editor
/// having broken, not as a bad argument.
#[test]
fn a_package_points_the_camera_at_a_place_without_touching_the_selection() {
    let proj = temp("lookat");
    install(
        &proj,
        "com.t.look",
        "",
        r#"
        ed.onUpdate(function()
            ed.lookAt(vec3(12, 1, -4))
            ed.lookAt({ 3, 0, 0 }, 2.5)
            local ok = pcall(ed.lookAt, vec3(0, 0, 0), 0)
            if ok then error("a zero distance was accepted") end
        end)
        "#,
    );
    let mut host = host_for(&proj);
    host.begin_frame(
        Snapshot { project_root: proj.clone(), ..Snapshot::default() },
        SceneMirror::build(&floptle_core::World::new(), &|_, _| None, &|_, _| None),
    );
    host.fire(HookKind::Update);
    assert!(host.packages[0].failed.is_none(), "{:?}", host.packages[0].failed);

    let cmds = host.take_cmds();
    assert!(
        cmds.iter().any(|c| matches!(c, ExtCmd::LookAt { at, distance: None }
            if *at == [12.0, 1.0, -4.0])),
        "{cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| matches!(c, ExtCmd::LookAt { at, distance: Some(d) }
            if *at == [3.0, 0.0, 0.0] && (*d - 2.5).abs() < 1e-9)),
        "{cmds:?}"
    );
    assert!(
        !cmds.iter().any(|c| matches!(c, ExtCmd::SelectionSet(_))),
        "looking at a place must not change what is selected: {cmds:?}"
    );
    let _ = std::fs::remove_dir_all(&proj);
}
