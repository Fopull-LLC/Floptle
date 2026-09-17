use super::*;

/// **What the driver feeds is what a script reads.**
///
/// The other `app` tests here all set a value and read it back, which passes
/// even if the initial state never arrives — a menu would open showing
/// defaults rather than the game's real settings, and only the controls
/// somebody touched would ever be right.
#[test]
fn a_menu_opens_showing_the_settings_the_game_actually_has() {
    let dir = std::env::temp_dir().join("floptle_script_test_app_initial");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "menu",
        "function update(node, dt)\n\
        \x20 print(app.title() .. \"|\" .. app.version() .. \"|\" .. app.vsync()\n\
        \x20   .. \"|\" .. tostring(app.retro()) .. \"|\" .. app.retroHeight())\n\
        end\n",
    );
    let (mut world, _e) = world_with_script("menu");
    let mut host = ScriptHost::new();
    host.set_app_info(crate::app_api::AppInfo {
        title: "Test Game".into(),
        version: "9.9.9".into(),
        vsync: crate::app_api::Vsync::Adaptive,
        retro: true,
        retro_height: 240,
        retro_integer_scale: false,
        fullscreen: false,
    });
    host.run(&mut world, &dir, 0.1, 0.1);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let said = host.drain_logs().into_iter().map(|l| l.msg).collect::<Vec<_>>().join("\n");
    assert!(
        said.contains("Test Game|9.9.9|Adaptive|true|240"),
        "a menu asked what the game is and got {said:?}"
    );
    // Reading changes nothing — a Video tab paints itself every frame.
    assert!(host.take_app_requests().is_empty());
}

/// **A settings menu reads back what it just set.**
///
/// The driver applies these a moment later — a swap chain and a GPU target
/// are not things a Lua call can touch — so if `app.vsync()` answered the
/// old value until then, every control in a Video tab would snap back to its
/// previous position for a frame after being clicked. That reads as a
/// control that did not work.
#[test]
fn a_settings_menu_reads_back_what_it_just_set() {
    let dir = std::env::temp_dir().join("floptle_script_test_app_settings");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "menu",
        "function update(node, dt)\n\
        \x20 app.setVsync(\"Off\")\n\
        \x20 app.setRetroHeight(360)\n\
        \x20 app.setRetroIntegerScale(true)\n\
        \x20 print(app.vsync() .. \"|\" .. app.retroHeight() .. \"|\" .. tostring(app.retroIntegerScale()))\n\
        end\n",
    );
    let (mut world, _e) = world_with_script("menu");
    let mut host = ScriptHost::new();
    host.set_app_info(crate::app_api::AppInfo {
        title: "Game".into(),
        version: "0.0.0".into(),
        vsync: crate::app_api::Vsync::On,
        retro: true,
        retro_height: 240,
        retro_integer_scale: false,
        fullscreen: false,
    });
    host.run(&mut world, &dir, 0.1, 0.1);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let said = host.drain_logs().into_iter().map(|l| l.msg).collect::<Vec<_>>().join("\n");
    assert!(
        said.contains("Off|360|true"),
        "a menu set three settings and read back {said:?} — it must see its own change"
    );
    // …and the driver is told to actually go and do it.
    let req = host.take_app_requests();
    assert_eq!(req.vsync, Some(crate::app_api::Vsync::Off));
    assert_eq!(req.retro_height, Some(360));
    assert_eq!(req.retro_integer_scale, Some(true));
    assert!(!req.quit, "nobody asked to quit");
    // Drained: a request left in the queue would be applied again every
    // frame, which for `quit` is the difference between closing once and
    // never being able to do anything else.
    assert!(host.take_app_requests().is_empty());
}

/// `app.quit()` reaches the driver as a request rather than doing anything
/// itself — there is no event loop to reach from inside a Lua call, and what
/// quitting means differs between a build, the editor and a headless run.
#[test]
fn quit_is_a_request_the_driver_answers() {
    let dir = std::env::temp_dir().join("floptle_script_test_app_quit");
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "menu", "function update(node, dt)\n  app.quit()\nend\n");
    let (mut world, _e) = world_with_script("menu");
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert!(host.take_app_requests().quit, "the driver was never told");
    assert!(!host.take_app_requests().quit, "and it must not be told twice");
}

/// A mode nobody recognises is named, not ignored. A settings menu that
/// silently kept the old value would be a control that appears to work.
#[test]
fn an_unknown_vsync_mode_is_refused_by_name() {
    let dir = std::env::temp_dir().join("floptle_script_test_app_badmode");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "menu",
        "function update(node, dt)\n\
        \x20 local ok, err = pcall(function() app.setVsync(\"vsync\") end)\n\
        \x20 print(tostring(ok) .. \"|\" .. tostring(err))\n\
        end\n",
    );
    let (mut world, _e) = world_with_script("menu");
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);
    let said = host.drain_logs().into_iter().map(|l| l.msg).collect::<Vec<_>>().join("\n");
    assert!(said.contains("false|"), "it was accepted: {said:?}");
    assert!(said.contains("\"On\""), "the refusal has to list the modes: {said:?}");
    assert!(host.take_app_requests().vsync.is_none(), "a refused mode must not be queued");
}

/// `findTagged(...)[0]` is the first hour of every Lua API, and the engine's
/// answer was a `nil` that died one call later as "attempt to index a nil
/// value" — pointing at the line, saying nothing about the cause. An index
/// below 1 is never an element of a Lua list, so it can say so outright.
/// A positive index past the end still reads nil: `if findTagged("x")[1]`
/// is how you ask whether there are any.
#[test]
fn indexing_a_result_list_from_zero_says_lists_start_at_one() {
    let dir = std::env::temp_dir().join("floptle_script_test_zero_index");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "zero",
        "function update(node, dt)\n  \
         if dt < 0.15 then\n    \
         if findTagged(\"me\")[9] == nil then node.y = 2 end\n  \
         else\n    local _ = findTagged(\"me\")[0]\n  end\n\
         end\n",
    );
    let (mut world, e) = world_with_script("zero");
    world.insert(e, floptle_core::Tags(vec!["me".into()]));
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);
    assert!(host.errors().is_empty(), "past-the-end must stay nil: {:?}", host.errors());
    assert_eq!(world.get::<Transform>(e).unwrap().translation.y, 2.0);
    host.run(&mut world, &dir, 0.2, 0.3);
    let errs = host.errors().join("\n");
    assert!(errs.contains("1-based") && errs.contains("[0]"), "{errs}");
}

/// A screen with a section switched off, written the way anybody writes it:
/// `local dead = nil` and then the section in the list. That leaves a hole
/// in the array, and a hole used to take the whole screen down — one absent
/// section and nothing at all was built, with an error naming an index
/// rather than a section. Found in a real project.
#[test]
fn a_section_switched_off_does_not_take_the_screen_with_it() {
    let dir = std::env::temp_dir().join(format!("floptle_uimake_nil_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "hud",
        "\
function update(node, dt)
  local vitals = { 'text', text = 'HP' }
  local dead   = nil                      -- the section that is not on screen
  local hint   = { 'text', text = 'HINT' }
  ui.make(node, { vitals, dead, hint })
end
",
    );
    let (mut world, e) = world_with_script("hud");
    world.insert(e, floptle_core::Matter::Empty);
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    host.apply_ui_makes(&mut world);
    assert_eq!(
        world.query::<floptle_core::Made>().count(),
        2,
        "the two sections that ARE on screen"
    );

    // …and the trailing half of the same problem: a hole truncates Lua's
    // length operator, so `hint` used to be dropped without a word.
    let texts: Vec<String> = world
        .query::<floptle_ui::ElementSpec>()
        .filter_map(|(_, s)| s.text.as_ref().map(|t| t.text.clone()))
        .collect();
    assert!(texts.contains(&"HINT".to_string()), "the section AFTER the hole: {texts:?}");

    // An empty table is how a screen is taken down. It used to describe one
    // anonymous box, so hiding a menu left an element behind every time.
    write_script(&dir, "hud", "function update(node, dt)\n  ui.make(node, {})\nend\n");
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    let destroy = host.apply_ui_makes(&mut world);
    assert_eq!(destroy.len(), 2, "both sections handed back for destruction");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `ui.make` raises on a property name it does not know, and the reasoning
/// is right: a declarative screen that silently ignores a line is worse
/// than one that stops. The same has to be true of a value.
///
/// `pin = "topCenter"` used to answer `topLeft`, silently and forever. Four
/// HUD elements — a floor readout, a controls hint, an interaction prompt
/// and every shop note — stacked into one corner underneath the panel that
/// legitimately lived there. The player's report was "the HUD is clipping
/// over things and covering the scene", which is a perfect description of
/// the symptom and points nowhere near the spelling that caused it.
#[test]
fn ui_make_refuses_a_value_a_property_does_not_take() {
    let dir = std::env::temp_dir().join(format!("floptle_uimake_val_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "hud",
        "function update(node, dt)\n  ui.make(node, { { 'text', text = 'X', pin = 'middle' } })\nend\n",
    );
    let (mut world, e) = world_with_script("hud");
    world.insert(e, floptle_core::Matter::Empty);
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    let err = host.errors().join(" ");
    assert!(!err.is_empty(), "a bad pin was accepted");
    // The message has to carry all three: which property, what it got, and
    // what it takes. Any one missing and you are back to re-reading a table.
    for want in ["pin", "middle", "topLeft", "bottomRight"] {
        assert!(err.contains(want), "the error never mentions {want}: {err}");
    }

    // …and the spelling people actually write is answered. This is the one
    // that was reported; refusing it would have been correct and useless.
    write_script(
        &dir,
        "hud",
        "function update(node, dt)\n  ui.make(node, { { 'text', text = 'X', pin = 'bottomCenter' } })\nend\n",
    );
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    host.apply_ui_makes(&mut world);
    let pinned = world
        .query::<floptle_ui::ElementSpec>()
        .filter_map(|(_, s)| match s.place {
            floptle_ui::Place::Pin { anchor, .. } => Some(anchor),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        pinned.contains(&floptle_ui::Anchor::Bottom),
        "bottomCenter did not land at the bottom: {pinned:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Reconcile reuses entities, so an element that was a buy button and is
/// now a sold-out label is the same entity with no `clicked` in its new
/// description. Its old closure used to stay armed — clicking one thing did
/// another thing's job, intermittently, depending on what the screen last
/// showed.
#[test]
fn an_element_that_stops_being_a_button_stops_answering_the_old_one() {
    let dir = std::env::temp_dir().join(format!("floptle_uimake_hook_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "shop",
        "\
function update(node, dt)
  ui.make(node, { { 'text', key = 'row', text = 'BUY',
                button = true, onClicked = function() log('FIRED') end } })
end
",
    );
    let (mut world, e) = world_with_script("shop");
    world.insert(e, floptle_core::Matter::Empty);
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    host.apply_ui_makes(&mut world);
    let row = world
        .query::<floptle_core::Made>()
        .find(|(_, m)| m.key == "row")
        .map(|(e, _)| e.index())
        .expect("the row");

    // It is a button, and it answers.
    let fired = |h: &mut ScriptHost| {
        h.drain_logs().iter().filter(|l| l.msg.contains("FIRED")).count()
    };
    let _ = fired(&mut host); // clear anything from the describe pass
    host.run_ui_hooks(&mut world, &[(row, "clicked")]);
    assert_eq!(fired(&mut host), 1, "the button works");

    // The same row, re-described as a plain label.
    write_script(
        &dir,
        "shop",
        "\
function update(node, dt)
  ui.make(node, { { 'text', key = 'row', text = 'SOLD OUT' } })
end
",
    );
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    host.apply_ui_makes(&mut world);
    let same = world
        .query::<floptle_core::Made>()
        .find(|(_, m)| m.key == "row")
        .map(|(e, _)| e.index());
    assert_eq!(same, Some(row), "reconcile kept the entity — that is the whole hazard");
    let _ = fired(&mut host);
    host.run_ui_hooks(&mut world, &[(row, "clicked")]);
    assert_eq!(fired(&mut host), 0, "…and it must NOT answer the old closure");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ui_hook_events_reach_the_node_scripts() {
    // A clicked/hoverStart event fires the same-named function on the node's
    // scripts, with a node handle argument; writes flush like any handle write.
    let dir = std::env::temp_dir().join("floptle_script_test_ui_hooks");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "btn",
        concat!(
            "function clicked(node)\n  node.y = node.y + 1\n",
            "  local c = node:getcomponent(\"UiElement\")\n",
            "  if c then c.opacity = 0.25 end\nend\n",
            "function hoverStart(node)\n  node.z = 7\nend\n",
        ),
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, floptle_core::Name("Play".into()));
    world.insert(
        e,
        floptle_ui::ElementSpec { button: true, ..Default::default() },
    );
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "btn".into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0); // builds the instance envs
    host.run_ui_hooks(&mut world, &[(e.index(), "hoverStart"), (e.index(), "clicked")]);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let tr = world.get::<Transform>(e).unwrap();
    assert_eq!((tr.translation.y, tr.translation.z), (1.0, 7.0));
    assert_eq!(world.get::<floptle_ui::ElementSpec>(e).unwrap().opacity, 0.25);
}

/// `ui.on(element, hook, fn)`: one manager script answers for buttons it
/// does not live on — the point of the whole thing, since the alternative
/// is a script file per button.
///
/// Also pins the two properties that make it safe to write: registering
/// again replaces (so calling it from `update` costs one closure, not one
/// per frame), and `ui.off` stops it.
#[test]
fn a_manager_hears_buttons_it_does_not_live_on() {
    let dir = std::env::temp_dir().join("floptle_script_test_ui_on");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "menu",
        concat!(
            "hits = 0\n",
            "last = \"\"\n",
            "lastEvent = \"\"\n",
            // Registered from `update`, deliberately: re-registering the
            // same (element, hook) must replace rather than stack.
            "function update(node, dt)\n",
            "  for i = 1, 2 do\n",
            "    ui.on(find(\"Btn\" .. i), \"clicked\", function(el, ev)\n",
            "      hits = hits + 1\n",
            "      last = el.name\n",
            "      lastEvent = ev\n",
            "    end)\n",
            "  end\n",
            "end\n",
            "function stopListening(node)\n  ui.off(find(\"Btn1\"))\nend\n",
        ),
    );
    let (mut world, menu, btns) = menu_world("menu", 2);
    let mut host = ScriptHost::new();
    for _ in 0..3 {
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    }
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let read = |host: &ScriptHost, key: &str| -> String {
        host.instance_env(menu.index(), "menu")
            .and_then(|e| e.get::<String>(key).ok())
            .unwrap_or_default()
    };
    let hits = |host: &ScriptHost| -> f64 {
        host.instance_env(menu.index(), "menu")
            .and_then(|e| e.get::<f64>("hits").ok())
            .unwrap_or_default()
    };

    host.run_ui_hooks(&mut world, &[(btns[1], "clicked")]);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(hits(&host), 1.0, "three frames of ui.on must leave ONE listener");
    assert_eq!(read(&host, "last"), "Btn2", "the element that fired is the argument");
    assert_eq!(read(&host, "lastEvent"), "clicked", "…and the hook name rides along");

    // A hook the manager never asked for reaches nothing.
    host.run_ui_hooks(&mut world, &[(btns[0], "hoverStart")]);
    assert_eq!(hits(&host), 1.0);
    host.run_ui_hooks(&mut world, &[(btns[0], "clicked")]);
    assert_eq!(hits(&host), 2.0);

    // `ui.off` — and only for the element named.
    host.call_action(&mut world, &dir, menu.index(), "menu", "stopListening");
    host.run_ui_hooks(&mut world, &[(btns[0], "clicked"), (btns[1], "clicked")]);
    assert_eq!(hits(&host), 3.0, "Btn1 is off, Btn2 still listening");
    assert_eq!(read(&host, "last"), "Btn2");
}

/// A listener dies with the script that registered it. Destroying a menu
/// manager must not leave its closures answering buttons — the closure also
/// holds that script's whole environment alive.
#[test]
fn a_listener_dies_with_the_script_that_registered_it() {
    let dir = std::env::temp_dir().join("floptle_script_test_ui_on_life");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "mgr",
        concat!(
            "hits = 0\n",
            "function update(node, dt)\n",
            "  ui.on(find(\"Btn1\"), \"clicked\", function() hits = hits + 1 end)\n",
            "end\n",
        ),
    );
    let (mut world, menu, btns) = menu_world("mgr", 1);
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    host.run_ui_hooks(&mut world, &[(btns[0], "clicked")]);
    let hits = |host: &ScriptHost| -> f64 {
        host.instance_env(menu.index(), "mgr")
            .and_then(|e| e.get::<f64>("hits").ok())
            .unwrap_or_default()
    };
    assert_eq!(hits(&host), 1.0);
    // The manager goes away (the driver reports what it destroyed).
    host.drop_ui_handlers(&[menu.index()]);
    host.run_ui_hooks(&mut world, &[(btns[0], "clicked")]);
    assert_eq!(hits(&host), 1.0, "a destroyed manager stops answering");
    // …and so does a listener whose element went away — entity indices are
    // reused, so a stale one would fire on whatever inherits the slot.
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0); // update re-registers
    host.drop_ui_handlers(&[btns[0]]);
    host.run_ui_hooks(&mut world, &[(btns[0], "clicked")]);
    assert_eq!(hits(&host), 1.0, "the element is gone, so nothing fires for it");
}

/// The other half: a script that would rather ask than be called back.
/// `ui.clicked(el)` / `ui.events()` read the same list the hooks fire from,
/// published before the run — so a poll and a hook can't disagree.
#[test]
fn this_frames_ui_events_can_be_polled() {
    let dir = std::env::temp_dir().join("floptle_script_test_ui_poll");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "poll",
        concat!(
            "clicks = \"\"\n",
            "seen = 0\n",
            "hover = \"\"\n",
            "function update(node, dt)\n",
            "  if ui.clicked(find(\"Btn1\")) then clicks = clicks .. \"1\" end\n",
            "  if ui.clicked(find(\"Btn2\")) then clicks = clicks .. \"2\" end\n",
            "  seen = #ui.events(\"clicked\")\n",
            "  local h = ui.hovered()\n",
            "  hover = h and h.name or \"\"\n",
            "  if ui.hovered(find(\"Btn2\")) then hover = hover .. \"!\" end\n",
            "end\n",
        ),
    );
    let (mut world, menu, btns) = menu_world("poll", 2);
    let mut host = ScriptHost::new();
    host.set_ui_frame_state(&[(btns[1], "clicked"), (btns[0], "hoverStart")], Some(btns[1]), None);
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let env = host.instance_env(menu.index(), "poll").expect("live instance");
    assert_eq!(env.get::<String>("clicks").unwrap(), "2");
    assert_eq!(env.get::<f64>("seen").unwrap(), 1.0, "hoverStart is not a click");
    assert_eq!(env.get::<String>("hover").unwrap(), "Btn2!");

    // Next frame, nothing happened: the answers are per-frame, not sticky.
    host.set_ui_frame_state(&[], None, None);
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    let env = host.instance_env(menu.index(), "poll").expect("live instance");
    assert_eq!(env.get::<String>("clicks").unwrap(), "2", "no new clicks");
    assert_eq!(env.get::<f64>("seen").unwrap(), 0.0);
    assert_eq!(env.get::<String>("hover").unwrap(), "");
}

/// Listening for a click on something that takes no clicks is the one
/// mistake this API makes easy, and it leaves nothing to look at. It warns.
#[test]
fn listening_to_an_element_that_takes_no_clicks_warns() {
    let dir = std::env::temp_dir().join("floptle_script_test_ui_on_warn");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "oops",
        concat!(
            "function start(node)\n",
            "  ui.on(find(\"Scenery\"), \"clicked\", function() end)\n",
            "  ui.on(find(\"Btn1\"), \"clicked\", function() end)\n",
            "end\n",
        ),
    );
    let (mut world, _menu, _btns) = menu_world("oops", 1);
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "a warning, not an error: {:?}", host.errors());
    let warnings: Vec<String> = host
        .drain_logs()
        .into_iter()
        .filter(|l| l.level == LogLevel::Warn)
        .map(|l| l.msg)
        .collect();
    assert_eq!(warnings.len(), 1, "only the wrong one warns: {warnings:?}");
    assert!(warnings[0].contains("Scenery"), "{}", warnings[0]);
    assert!(warnings[0].contains("Button"), "it names the fix: {}", warnings[0]);
    // Warned once, at registration — not every frame it fails to fire.
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    assert!(host.drain_logs().iter().all(|l| l.level != LogLevel::Warn), "warned once");
}

/// `ui.make` end to end: a Lua table becomes real nodes, a described
/// button's inline `onClicked` fires on a click, and re-describing the
/// screen with one fewer row destroys exactly that row.
///
/// The pieces have their own tests; this one is the whole path, because
/// that is where the seams are — the parse hands paths to the reconcile,
/// the reconcile hands entities back, and the closures have to end up on
/// the right ones.
#[test]
fn ui_make_builds_a_screen_and_its_buttons_work() {
    let dir = std::env::temp_dir().join("floptle_script_test_ui_make");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "screen",
        concat!(
            "crew = { \"ana\", \"bo\", \"cy\" }\n",
            "picked = \"\"\n",
            "function build(node)\n",
            "  ui.make(node, { \"col\", gap = 6, items = crew,\n",
            "    function(id) return { \"button\", key = id, text = id,\n",
            "      onClicked = function(n) picked = id end } end })\n",
            "end\n",
            "function start(node) build(node) end\n",
        ),
    );
    let mut world = World::default();
    let panel = world.spawn();
    world.insert(panel, Transform::IDENTITY);
    world.insert(panel, floptle_core::Name("Panel".into()));
    world.insert(panel, floptle_ui::ElementSpec::default());
    world.insert(
        panel,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "screen".into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.apply_ui_makes(&mut world).is_empty(), "nothing to destroy on the first build");
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());

    // A column under the panel, three buttons under that.
    let made: Vec<floptle_core::Entity> =
        world.query::<floptle_core::Made>().map(|(e, _)| e).collect();
    assert_eq!(made.len(), 4, "one column + three rows");
    let mut rows: Vec<(u32, floptle_core::Entity)> = world
        .query::<floptle_core::Made>()
        .filter(|(_, m)| m.kind == "button")
        .map(|(e, m)| (m.slot, e))
        .collect();
    rows.sort_by_key(|(slot, e)| (*slot, e.index()));
    assert_eq!(rows.len(), 3);
    let texts: Vec<String> = rows
        .iter()
        .map(|(_, e)| {
            world.get::<floptle_ui::ElementSpec>(*e).unwrap().text.as_ref().unwrap().text.clone()
        })
        .collect();
    assert_eq!(texts, vec!["ana", "bo", "cy"]);

    // The middle row's inline closure runs on a click, with no script
    // file, no prefab and no `clicked` function anywhere.
    host.run_ui_hooks(&mut world, &[(rows[1].1.index(), "clicked")]);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let picked: String = host
        .instance_env(panel.index(), "screen")
        .and_then(|env| env.get::<String>("picked").ok())
        .unwrap_or_default();
    assert_eq!(picked, "bo");

    // Describing the same screen again changes nothing…
    host.call_action(&mut world, &dir, panel.index(), "screen", "build");
    assert!(host.apply_ui_makes(&mut world).is_empty(), "a re-render must not churn");
    assert_eq!(world.query::<floptle_core::Made>().count(), 4);

    // …and dropping a row hands back exactly that row.
    let env = host.instance_env(panel.index(), "screen").expect("the instance is live");
    env.set("crew", vec!["ana".to_string(), "cy".to_string()]).unwrap();
    host.call_action(&mut world, &dir, panel.index(), "screen", "build");
    assert_eq!(host.apply_ui_makes(&mut world), vec![rows[1].1.index()]);
}

/// `node:setShaderParam` lands in the UI element's `shader_params` when it
/// carries a `stage ui` shader, and in the Material's otherwise — the
/// bridge instruments (navball) drive their uniforms through.
#[test]
fn set_shader_param_reaches_element_and_material() {
    let dir = std::env::temp_dir().join("floptle_script_test_shader_param");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "inst",
        concat!(
            "function update(node, dt)\n",
            "  node:setShaderParam(\"nose\", 0.1, 0.9, 0.2)\n",
            "  local m = find(\"Meshy\")\n",
            "  m:setShaderParam(\"glow\", 2.5)\n",
            "end\n",
        ),
    );
    let mut world = World::default();
    let ball = world.spawn();
    world.insert(ball, Transform::IDENTITY);
    world.insert(ball, floptle_core::Name("Ball".into()));
    world.insert(
        ball,
        floptle_ui::ElementSpec { shader: "shaders/navball.flsl".into(), ..Default::default() },
    );
    world.insert(
        ball,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "inst".into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );
    let meshy = world.spawn();
    world.insert(meshy, Transform::IDENTITY);
    world.insert(meshy, floptle_core::Name("Meshy".into()));
    world.insert(meshy, Material { shader: Some("shaders/x.flsl".into()), ..Default::default() });
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let spec = world.get::<floptle_ui::ElementSpec>(ball).unwrap();
    assert_eq!(spec.shader_params.get("nose"), Some(&[0.1, 0.9, 0.2, 0.0]));
    let mat = world.get::<Material>(meshy).unwrap();
    assert_eq!(mat.shader_params.get("glow"), Some(&[2.5, 0.0, 0.0, 0.0]));
}

#[test]
fn script_drives_ui_text_slider_and_element_fields() {
    // The HUD path: node.text swaps a label, getcomponent("UiSlider").value
    // drives a health bar, getcomponent("UiElement") reaches visibility etc.
    let dir = std::env::temp_dir().join("floptle_script_test_ui");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "hud",
        concat!(
            "function update(node, dt)\n",
            "  local label = find(\"HpLabel\")\n",
            "  label.text = 42\n",
            "  local bar = find(\"HpBar\")\n",
            "  bar:getcomponent(\"UiSlider\").value = 25\n",
            "  bar:getcomponent(\"UiElement\").opacity = 0.5\n",
            "  node.x = (label.text == \"42\" and 1 or 0)\n",
            "end\n",
        ),
    );
    let mut world = World::default();
    let driver = world.spawn();
    world.insert(driver, Transform::IDENTITY);
    world.insert(
        driver,
        Scripts(vec![floptle_core::ScriptInst { kind: "hud".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
    );
    let label = world.spawn();
    world.insert(label, Transform::IDENTITY);
    world.insert(label, floptle_core::Name("HpLabel".into()));
    world.insert(
        label,
        floptle_ui::ElementSpec {
            text: Some(floptle_ui::TextSpec { text: "hp".into(), ..Default::default() }),
            ..Default::default()
        },
    );
    let bar = world.spawn();
    world.insert(bar, Transform::IDENTITY);
    world.insert(bar, floptle_core::Name("HpBar".into()));
    world.insert(
        bar,
        floptle_ui::ElementSpec {
            slider: Some(floptle_ui::SliderSpec::default()),
            ..Default::default()
        },
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let lspec = world.get::<floptle_ui::ElementSpec>(label).unwrap();
    assert_eq!(lspec.text.as_ref().unwrap().text, "42");
    let bspec = world.get::<floptle_ui::ElementSpec>(bar).unwrap();
    assert_eq!(bspec.slider.unwrap().value, 25.0);
    assert_eq!(bspec.opacity, 0.5);
    // Read-your-writes: the script saw its own label.text assignment.
    assert_eq!(world.get::<Transform>(driver).unwrap().translation.x, 1.0);
}

/// `node.style`, `disabled` and `selected` — the state channel a menu
/// drives. Read-your-writes matters here as much as it does for `text`:
/// a row script routinely sets a style and then reads it back to decide
/// what else to do.
#[test]
fn script_drives_ui_style_and_states() {
    let dir = std::env::temp_dir().join("floptle_script_test_ui_style");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "row",
        concat!(
            "function update(node, dt)\n",
            "  local r = find(\"Row\")\n",
            "  r.style = \"button/danger\"\n",
            "  local e = r:getcomponent(\"UiElement\")\n",
            "  e.selected = 1\n",
            "  e.disabled = 0\n",
            "  node.x = (r.style == \"button/danger\") and 1 or 0\n",
            "end\n",
        ),
    );
    let mut world = World::default();
    let driver = world.spawn();
    world.insert(driver, Transform::IDENTITY);
    world.insert(
        driver,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "row".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let row = world.spawn();
    world.insert(row, Transform::IDENTITY);
    world.insert(row, floptle_core::Name("Row".into()));
    world.insert(
        row,
        floptle_ui::ElementSpec { style: "row".into(), disabled: true, ..Default::default() },
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let spec = world.get::<floptle_ui::ElementSpec>(row).unwrap();
    assert_eq!(spec.style, "button/danger");
    assert!(spec.selected);
    assert!(!spec.disabled);
    assert_eq!(
        world.get::<Transform>(driver).unwrap().translation.x,
        1.0,
        "the script must read back its own style write within the frame"
    );
}

#[test]
fn script_reads_and_moves_ui_focus() {
    let dir = std::env::temp_dir().join("floptle_script_test_ui_focus");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "menu",
        concat!(
            "function update(node, dt)\n",
            "  local play = find(\"Play\")\n",
            "  local quit = find(\"Quit\")\n",
            // Read the engine's focus, two ways.
            "  node.x = play.focused and 1 or 0\n",
            "  node.y = (ui.focused() ~= nil) and 1 or 0\n",
            // Move it, then read the move back within the same frame.
            "  ui.focus(quit)\n",
            "  node.z = quit.focused and 1 or 0\n",
            "end\n",
        ),
    );
    let mut world = World::default();
    let driver = world.spawn();
    world.insert(driver, Transform::IDENTITY);
    world.insert(
        driver,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "menu".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut el = |name: &str| {
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, floptle_core::Name(name.into()));
        world.insert(
            e,
            floptle_ui::ElementSpec { focusable: true, ..Default::default() },
        );
        e
    };
    let play = el("Play");
    let quit = el("Quit");

    let mut host = ScriptHost::new();
    // The engine publishes the focus before the run, exactly as the
    // interact pass does.
    host.set_ui_focus(Some(play.index()));
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let t = world.get::<Transform>(driver).unwrap().translation;
    assert_eq!(t.x, 1.0, "node.focused sees the engine's focus");
    assert_eq!(t.y, 1.0, "ui.focused() returns a node");
    assert_eq!(t.z, 1.0, "ui.focus() reads back within the same frame");
    // …and the engine gets the request out.
    assert_eq!(host.take_ui_focus_request(), Some(Some(quit.index())));
    assert_eq!(host.take_ui_focus_request(), None, "draining is one-shot");
}

#[test]
fn ui_bind_keeps_a_label_and_a_bar_up_to_date() {
    let dir = std::env::temp_dir().join("floptle_script_test_ui_bind");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "hud",
        concat!(
            "hp = 10\n",
            "function start(node)\n",
            // Say the relationship once, in `start` — not an `update` per
            // label that has to be kept true by hand.
            "  ui.bind(find(\"Label\"), \"text\", function() return \"HP \" .. hp end)\n",
            "  ui.bind(find(\"Bar\"), \"value\", function() return hp / 20 end)\n",
            "  ui.bind(find(\"Label\"), \"textColor\",\n",
            "          function() return hp >= 5 and color(1,1,1) or color(1,0,0) end)\n",
            "end\n",
            "function update(node, dt)\n  hp = hp - 5\nend\n",
        ),
    );
    let mut world = World::default();
    let driver = world.spawn();
    world.insert(driver, Transform::IDENTITY);
    world.insert(
        driver,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "hud".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let label = world.spawn();
    world.insert(label, Transform::IDENTITY);
    world.insert(label, floptle_core::Name("Label".into()));
    world.insert(
        label,
        floptle_ui::ElementSpec {
            text: Some(floptle_ui::TextSpec { text: "?".into(), ..Default::default() }),
            ..Default::default()
        },
    );
    let bar = world.spawn();
    world.insert(bar, Transform::IDENTITY);
    world.insert(bar, floptle_core::Name("Bar".into()));
    world.insert(
        bar,
        floptle_ui::ElementSpec {
            slider: Some(floptle_ui::SliderSpec { value: 0.0, ..Default::default() }),
            ..Default::default()
        },
    );

    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    // Bindings run after every `update`, so the first frame already shows
    // the value this frame produced, not the one it started with.
    assert_eq!(world.get::<floptle_ui::ElementSpec>(label).unwrap().text.as_ref().unwrap().text, "HP 5");
    let v = world.get::<floptle_ui::ElementSpec>(bar).unwrap().slider.unwrap().value;
    assert!((v - 0.25).abs() < 1e-6, "the bar found UiSlider.value, not UiElement: {v}");
    assert_eq!(
        world.get::<floptle_ui::ElementSpec>(label).unwrap().text.as_ref().unwrap().color,
        [1.0, 1.0, 1.0, 1.0]
    );

    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert_eq!(world.get::<floptle_ui::ElementSpec>(label).unwrap().text.as_ref().unwrap().text, "HP 0");
    assert_eq!(
        world.get::<floptle_ui::ElementSpec>(label).unwrap().text.as_ref().unwrap().color,
        [1.0, 0.0, 0.0, 1.0],
        "the colour binding re-evaluated too"
    );
}

#[test]
fn a_binding_that_throws_is_dropped_rather_than_reported_sixty_times_a_second() {
    let dir = std::env::temp_dir().join("floptle_script_test_ui_bind_err");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "bad",
        concat!(
            "function start(node)\n",
            "  ui.bind(find(\"Label\"), \"text\", function() error(\"nope\") end)\n",
            "end\n",
        ),
    );
    let mut world = World::default();
    let driver = world.spawn();
    world.insert(driver, Transform::IDENTITY);
    world.insert(
        driver,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "bad".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let label = world.spawn();
    world.insert(label, Transform::IDENTITY);
    world.insert(label, floptle_core::Name("Label".into()));
    world.insert(
        label,
        floptle_ui::ElementSpec {
            text: Some(floptle_ui::TextSpec::default()),
            ..Default::default()
        },
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert_eq!(host.errors().len(), 1, "reported once: {:?}", host.errors());
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "…and not again: {:?}", host.errors());
}

/// `node.texture = "..."` did nothing — not an error, not a
/// warning, no return value. A character-select strip assigned portraits
/// that way for months and showed the placeholder on every slot.
#[test]
fn script_sets_and_reads_a_ui_element_texture() {
    let dir = std::env::temp_dir().join("floptle_script_test_ui_texture");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "portrait",
        "function update(node, dt)\n  \
           node.texture = \"textures/ui/sae.png\"\n  \
           readback = node.texture\n\
         end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    // A bare element with no image slot — the write has to create one, the
    // way a sprite frame-swap track does.
    world.insert(e, floptle_ui::ElementSpec::default());
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "portrait".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let spec = world.get::<floptle_ui::ElementSpec>(e).expect("element");
    assert_eq!(
        spec.image.as_ref().map(|i| i.texture.as_str()),
        Some("textures/ui/sae.png"),
        "the write must reach the ECS, not vanish"
    );
}

#[test]
fn string_field_applier_swaps_ui_image() {
    // The animation system's property tracks apply through these. A UI
    // image swap is the headline case (sprite frame-swapping).
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, floptle_ui::ElementSpec::default());

    // No image slot yet → the applier creates one.
    crate::apply_component_field_str(&mut world, e, "UiElement", "image", "textures/a.png");
    let img = world.get::<floptle_ui::ElementSpec>(e).unwrap().image.clone().unwrap();
    assert_eq!(img.texture, "textures/a.png");

    // A later frame swaps the texture on the existing slot.
    crate::apply_component_field_str(&mut world, e, "UiElement", "image", "textures/b.png");
    let img = world.get::<floptle_ui::ElementSpec>(e).unwrap().image.clone().unwrap();
    assert_eq!(img.texture, "textures/b.png");

    // The numeric applier still drives opacity on the same element.
    crate::apply_component_field(&mut world, e, "UiElement", "opacity", 0.5);
    assert_eq!(world.get::<floptle_ui::ElementSpec>(e).unwrap().opacity, 0.5);
}
