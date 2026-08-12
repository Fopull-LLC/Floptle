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
    let _ = ctx.run_ui(egui::RawInput::default(), |ui| {
        host.draw_window(which, ui);
    });
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
        walk("http.", http)
        walk("sys.", sys)
        walk("ed.prefs.", ed.prefs)
        table.sort(names)
        ed.log(table.concat(names, " "))
        "#,
    );
    let host = host_for(&proj);
    let log = host.take_log();
    assert!(!log.is_empty(), "the package did not run");
    let doc = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/editor-scripting.md"),
    )
    .expect("docs/editor-scripting.md");

    // The hooks are documented as a table of `ed.onX(fn)` rows and the stores as
    // one shared row, so the bare member name is what has to appear.
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
    let missing: Vec<&str> =
        log[0].msg.split_whitespace().filter(|n| !doc.contains(*n)).collect();
    assert!(
        missing.is_empty(),
        "gui widget(s) missing from docs/editor-scripting.md: {}",
        missing.join(", ")
    );
    let _ = std::fs::remove_dir_all(&proj);
}

/// The example package in `packages/scene-report/` must actually work: it is
/// what somebody reads to learn the API, and an example that raises teaches the
/// wrong thing. Installed into a temp project, drawn, and every hook fired.
#[test]
fn the_example_package_loads_draws_and_survives_every_hook() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packages/scene-report");
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
