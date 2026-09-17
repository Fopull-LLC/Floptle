use super::*;

/// **`net.isDedicated()` answers the state, both ways.**
///
/// The other half of `dedicated.rs`'s wiring guard: that one proves a
/// dedicated server sets the flag, this proves the binding reports it — and
/// crucially that it reports false for a player who is hosting. A version
/// that returned `net.isServer()` would pass a test that only checked the
/// true case, and it would be the original bug exactly: a hosting player
/// told they are a dedicated server drops out of their own lobby.
#[test]
fn is_dedicated_is_true_only_for_a_server_with_nobody_at_it() {
    let dir = std::env::temp_dir().join(format!("floptle-isded-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "probe",
        "function update(node)\n\
         \x20 print(tostring(net.isServer()) .. \",\" .. tostring(net.isDedicated()))\n\
         end\n",
    );

    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "probe".into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();

    let saw = |host: &mut ScriptHost, world: &mut World| -> String {
        host.run(world, &dir, 1.0 / 60.0, 1.0 / 60.0);
        host.drain_logs().iter().map(|l| l.msg.clone()).collect::<Vec<_>>().join("\n")
    };

    // A player hosting the game they are in: the server, and not dedicated.
    host.set_net_state(NetState {
        role: NetRoleState::Server,
        dedicated: false,
        ..Default::default()
    });
    let out = saw(&mut host, &mut world);
    assert!(
        out.contains("true,false"),
        "a hosting player must not be told they are a dedicated server: {out}"
    );

    // A server with nobody at it: both true.
    host.set_net_state(NetState {
        role: NetRoleState::Server,
        dedicated: true,
        ..Default::default()
    });
    let out = saw(&mut host, &mut world);
    assert!(out.contains("true,true"), "a dedicated server is still the server: {out}");

    // And offline is neither.
    host.set_net_state(NetState::default());
    let out = saw(&mut host, &mut world);
    assert!(out.contains("false,false"), "offline is not a dedicated server: {out}");
}

#[test]
fn net_bridge_rpc_synced_events_round_trip() {
    // The Lua net.* bridge (docs/multiplayer.md §8): rpc queueing with
    // guardrails, replicated→synced declaration + collect/apply, onRpc
    // dispatch with sender, and net.on event handlers.
    let dir = std::env::temp_dir().join("floptle_script_test_net");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "netty",
        "replicated = { hp = 100, name = \"flop\" }\n\
         joined = 0\n\
         function start(node)\n  net.on(\"playerJoined\", function(p) joined = p end)\nend\n\
         function update(node, dt)\n\
           if time < 0.02 then\n\
             net.rpc(\"hello\", { x = 1 })\n\
             net.rpc(\"too_big\", string.rep(\"x\", 2000))\n\
           end\n\
         end\n\
         onRpc = {}\n\
         function onRpc.hurt(args, sender)\n  synced.hp = synced.hp - args.dmg\n  node.x = sender\nend\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst { kind: "netty".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
    );
    let mut host = ScriptHost::new();
    host.set_net_state(NetState {
        role: NetRoleState::Server,
        peers: vec![1],
        rtt_ms: 20.0,
        my_peer: None,
        ..Default::default()
    });
    host.run(&mut world, &dir, 0.01, 0.01);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());

    // rpc queue: "hello" queued; the oversized one dropped with a warning.
    let cmds = host.take_net_commands();
    let rpcs: Vec<_> = cmds
        .iter()
        .filter_map(|c| match c {
            NetCmd::Rpc { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(rpcs, vec!["hello".to_string()], "guarded rpc must drop, got {cmds:?}");
    assert!(
        host.drain_logs().iter().any(|l| l.level == LogLevel::Warn && l.msg.contains("too_big")),
        "oversized rpc must warn"
    );

    // synced: declared values collected (sorted), server-side.
    let collected = host.collect_synced();
    assert_eq!(collected.len(), 1);
    assert_eq!(collected[0].1, "netty");
    assert_eq!(
        collected[0].2,
        vec![
            ("hp".to_string(), floptle_net::NetValue::Num(100.0)),
            ("name".to_string(), floptle_net::NetValue::Str("flop".into())),
        ]
    );

    // onRpc dispatch mutates synced + gets the stamped sender.
    host.dispatch_rpc(
        &mut world,
        "hurt",
        &floptle_net::NetValue::Table(vec![(
            floptle_net::NetValue::Str("dmg".into()),
            floptle_net::NetValue::Num(25.0),
        )]),
        7,
    );
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let collected = host.collect_synced();
    assert_eq!(collected[0].2[0], ("hp".to_string(), floptle_net::NetValue::Num(75.0)));
    assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 7.0, "sender reaches Lua");

    // apply_synced (the client path) overwrites the store.
    host.apply_synced(e.index(), "netty", &[("hp".into(), floptle_net::NetValue::Num(10.0))]);
    let collected = host.collect_synced();
    assert_eq!(collected[0].2[0], ("hp".to_string(), floptle_net::NetValue::Num(10.0)));

    // net.on handler fires with the peer id.
    host.fire_net_event(&mut world, "playerJoined", Some(42), None);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    // (joined lives in the env; verify indirectly — no error + no crash is
    // the contract here; value-level checks ride the rpc/synced paths above.)

    // Client-side writes to synced warn.
    host.set_net_state(NetState { role: NetRoleState::Client, peers: vec![], rtt_ms: 0.0, my_peer: Some(7), ..Default::default() });
    host.dispatch_rpc(
        &mut world,
        "hurt",
        &floptle_net::NetValue::Table(vec![(
            floptle_net::NetValue::Str("dmg".into()),
            floptle_net::NetValue::Num(1.0),
        )]),
        0,
    );
    assert!(
        host.drain_logs().iter().any(|l| l.level == LogLevel::Warn && l.msg.contains("synced.hp")),
        "client synced write must warn"
    );
}

/// A game has to be able to tell its own players the lobby code.
///
/// The relay hands it back at a moment only the engine sees, so a front end
/// that couldn't read it had nowhere to get it — every game shipping a lobby
/// screen had to send players to the engine's own debug panel to find out
/// how their friends were supposed to join.
#[test]
fn a_lobby_screen_can_read_the_code_the_relay_handed_back() {
    let dir = std::env::temp_dir().join("floptle_script_test_lobby_code");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "lobby",
        "replicated = { code = \"\" }\n\
         function update(node, dt)\n  synced.code = net.lobbyCode() or \"waiting\"\nend\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst { kind: "lobby".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
    );
    let mut host = ScriptHost::new();

    // Before the relay answers: nil, so a lobby screen must poll rather
    // than read once. This is the state a host sits in for a round trip.
    host.set_net_state(NetState {
        role: NetRoleState::Server,
        peers: vec![],
        rtt_ms: 0.0,
        my_peer: None,
        ..Default::default()
    });
    host.run(&mut world, &dir, 0.016, 0.016);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let code = |h: &mut ScriptHost| match &h.collect_synced()[0].2[0].1 {
        floptle_net::NetValue::Str(s) => s.clone(),
        other => panic!("expected a string, got {other:?}"),
    };
    assert_eq!(code(&mut host), "waiting");

    // The relay answers.
    host.set_net_state(NetState {
        role: NetRoleState::Server,
        peers: vec![],
        rtt_ms: 0.0,
        my_peer: None,
        lobby_code: Some("QK7RM".into()),
        ..Default::default()
    });
    host.run(&mut world, &dir, 0.016, 0.032);
    assert_eq!(
        code(&mut host),
        "QK7RM",
        "the code must reach Lua, or a game cannot show it to the players who need it"
    );
}

/// A mistyped lobby code has to reach the player as words.
///
/// It is the most common thing that will ever go wrong in an online
/// session, and it used to arrive as an event indistinguishable from the
/// opponent closing their laptop — the relay's own explanation was
/// discarded one line below the pipe built to carry it.
#[test]
fn a_refused_join_reaches_the_game_with_the_relays_own_words() {
    let dir = std::env::temp_dir().join("floptle_script_test_join_state");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "lobby",
        "replicated = { shown = \"\" }\n\
         function update(node, dt)\n\
         \x20 local st, why = net.joinState()\n\
         \x20 synced.shown = why or st\n\
         end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst { kind: "lobby".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
    );
    let mut host = ScriptHost::new();
    let shown = |h: &mut ScriptHost| match &h.collect_synced()[0].2[0].1 {
        floptle_net::NetValue::Str(s) => s.clone(),
        other => panic!("expected a string, got {other:?}"),
    };

    // Offline: no join in progress, and it says so rather than "".
    host.run(&mut world, &dir, 0.016, 0.016);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(shown(&mut host), "offline");

    // The join is in flight. This is the state a game used to be unable to
    // tell apart from success, because role already reads "client" here.
    host.set_net_state(NetState {
        role: NetRoleState::Client,
        join_state: "connecting",
        ..Default::default()
    });
    host.run(&mut world, &dir, 0.016, 0.032);
    assert_eq!(shown(&mut host), "connecting");

    // The relay answers. The game can print this.
    host.set_net_state(NetState {
        role: NetRoleState::Client,
        join_state: "refused",
        join_error: Some("no lobby QK7RM".into()),
        ..Default::default()
    });
    host.run(&mut world, &dir, 0.016, 0.048);
    assert_eq!(
        shown(&mut host),
        "no lobby QK7RM",
        "the relay's reason must reach Lua — without it a wrong code and a \
         dropped connection are the same event"
    );
}

/// a driver owns a node's ticks, not its frames.
///
/// `extend_filters` used to put the node in `script_skip`, which gates every
/// pass — and the rollback driver replays only `fixedUpdate` and `update`.
/// So `lateUpdate` had no substitute execution anywhere and simply stopped,
/// with no error and no log line. It is the documented place to write a
/// node's cosmetic transform (it runs after the interpolated writeback), so
/// a game following that advice broke the instant the node went Rollback —
/// and only in a net match, never offline.
#[test]
fn a_driver_owned_node_still_gets_its_late_pass() {
    let dir = std::env::temp_dir().join("floptle_script_test_driver_late");
    let _ = std::fs::create_dir_all(&dir);
    // Each pass counts itself in a distinct axis: x = update, y = fixedUpdate,
    // z = lateUpdate.
    write_script(
        &dir,
        "counter",
        "function update(node, dt)\n  node.x = node.x + 1\nend\n\
         function fixedUpdate(node, dt)\n  node.y = node.y + 1\nend\n\
         function lateUpdate(node, dt)\n  node.z = node.z + 1\nend\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "counter".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    let pos = |w: &World| {
        let t = w.get::<Transform>(e).unwrap().translation;
        (t.x, t.y, t.z)
    };

    // Unclaimed: every pass runs globally.
    host.run(&mut world, &dir, 0.016, 0.016);
    host.run_fixed(&mut world, 0.016, 0.016);
    host.run_late(&mut world, 0.016, 0.016);
    assert_eq!(pos(&world), (1.0, 1.0, 1.0), "all three passes run when unclaimed");

    // The driver claims it. Its ticks move into the driver — but its late
    // pass has no substitute anywhere, so it must keep running here.
    host.extend_filters([e.index()]);
    assert!(host.is_filtered(e.index()), "the driver owns it");
    host.run(&mut world, &dir, 0.016, 0.032);
    host.run_fixed(&mut world, 0.016, 0.032);
    host.run_late(&mut world, 0.016, 0.032);
    assert_eq!(
        pos(&world),
        (1.0, 1.0, 2.0),
        "update/fixedUpdate are the driver's now; lateUpdate ran exactly once more"
    );

    // The driver's own substitute calls still bypass the filter.
    host.run_frame_for(&mut world, e.index(), 1.0 / 60.0, 0.048);
    host.run_fixed_for(&mut world, e.index(), 1.0 / 60.0, 0.048);
    assert_eq!(pos(&world), (2.0, 2.0, 2.0), "the driver replays the ticks it owns");

    // Released: back to every pass globally, exactly once.
    host.shrink_filters([e.index()]);
    assert!(!host.is_filtered(e.index()));
    host.run(&mut world, &dir, 0.016, 0.064);
    host.run_fixed(&mut world, 0.016, 0.064);
    host.run_late(&mut world, 0.016, 0.064);
    assert_eq!(pos(&world), (3.0, 3.0, 3.0), "handed back cleanly");
}

/// The other reason a node is filtered must keep its old meaning: a
/// snapshot-driven node is not simulated locally at all, so every pass —
/// `lateUpdate` included — stays skipped. Separating the two sets must not
/// leak the late pass into this case.
#[test]
fn a_snapshot_driven_node_still_skips_every_pass() {
    let dir = std::env::temp_dir().join("floptle_script_test_snapshot_late");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "late_only",
        "function lateUpdate(node, dt)\n  node.z = node.z + 1\nend\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "late_only".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.016, 0.016);
    host.run_late(&mut world, 0.016, 0.016);
    assert_eq!(world.get::<Transform>(e).unwrap().translation.z, 1.0);

    let mut skip = std::collections::HashSet::new();
    skip.insert(e.index());
    host.set_script_filter(skip);
    host.run_late(&mut world, 0.016, 0.032);
    assert_eq!(
        world.get::<Transform>(e).unwrap().translation.z,
        1.0,
        "a server-authoritative node runs NO pass locally, late included"
    );
}

/// field regression: a node in `script_skip` never gets a
/// late pass, and a client's join sequence puts every rollback fighter
/// there before the driver exists to claim it back.
///
/// `script_skip` gates every pass; `driver_skip` gates all but `lateUpdate`,
/// because no driver replays the late pass. The join sequence writes the
/// first and the rollback start writes the second, and for two releases
/// nothing took the fighters back out of the first — so the fight ran and
/// the cosmetic pass silently did not, on the client only.
#[test]
fn a_driver_owned_node_keeps_its_late_pass_after_the_session_filtered_it() {
    let dir = std::env::temp_dir().join("floptle_script_test_late_filter");
    let _ = std::fs::create_dir_all(&dir);
    // `fixedUpdate` writes one value, `lateUpdate` writes another over it —
    // the same shape the field report measured with (+0.25 on top).
    write_script(
        &dir,
        "facing",
        concat!(
            "function fixedUpdate(node, dt)\n  node.y = 1\nend\n",
            "function lateUpdate(node, dt)\n  node.y = 2\nend\n",
        ),
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "facing".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);

    // Step 2 of the join: no driver yet, so the session classifies the
    // fighter as an ordinary synced node and filters it out of everything.
    host.set_script_filter(std::collections::HashSet::from([e.index()]));
    host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
    host.run_late(&mut world, 1.0 / 60.0, 0.0);
    assert_eq!(
        world.get::<Transform>(e).unwrap().translation.y,
        0.0,
        "the un-driven window is supposed to skip everything; if it doesn't, \
         this test proves nothing"
    );

    // Step 3: the driver binds it. `extend_filters` alone is the bug —
    // `script_skip` still holds it, so the late pass stays dead.
    host.extend_filters([e.index()]);
    host.run_late(&mut world, 1.0 / 60.0, 0.0);
    assert_eq!(
        world.get::<Transform>(e).unwrap().translation.y,
        0.0,
        "reproducing the bug: extend_filters does not undo script_skip"
    );

    // The fix: the rollback start takes the session's half back out first.
    host.shrink_filters([e.index()]);
    host.extend_filters([e.index()]);
    host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
    assert_eq!(
        world.get::<Transform>(e).unwrap().translation.y,
        0.0,
        "the driver still owns the TICK — the global fixedUpdate stays off"
    );
    host.run_late(&mut world, 1.0 / 60.0, 0.0);
    assert_eq!(
        world.get::<Transform>(e).unwrap().translation.y,
        2.0,
        "…and lateUpdate runs again, which is the whole point"
    );
}

/// A4 scheduler: tick-driven determinism, cancel, tween endpoints — and the
/// invariant that targeted replays (`run_fixed_for`) do not advance timers
/// (netcode prediction re-runs one entity's tick; a scheduler advancing
/// there would double-fire everything pending).
#[test]
fn scheduler_fires_on_ticks_and_ignores_replays() {
    let dir = std::env::temp_dir().join("floptle_script_test_sched");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "sched",
        "local fired, everies, tw_last, tw_calls = 0, 0, -1, 0\n\
         local cancelled_ran = false\n\
         function start(node)\n\
           after(0.045, function() fired = fired + 1 end)\n\
           local h = after(0.045, function() cancelled_ran = true end)\n\
           h:cancel()\n\
           every(0.095, function() everies = everies + 1 end)\n\
           tween(0.1, function(a) tw_last = a; tw_calls = tw_calls + 1 end, \"smooth\")\n\
         end\n\
         function update(node, dt)\n\
           node.x = fired\n\
           node.y = everies + (cancelled_ran and 100 or 0)\n\
           node.z = tw_last * 1000 + tw_calls\n\
         end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "sched".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    let dt = 1.0 / 60.0;
    host.run(&mut world, &dir, dt, 0.0); // start() schedules everything
    // 30 global ticks = 0.5s: after(0.045) fired once, every(0.095) fired 5
    // times (0.095, 0.19, 0.285, 0.38, 0.475 — periods deliberately off the
    // tick grid so f64 accumulation can't make the count edge-dependent),
    // and the 0.1s tween completed, ending exactly at eased(1.0) = 1.0.
    for i in 0..30 {
        host.run_fixed(&mut world, dt, i as f32 * dt);
    }
    // Replays must not advance the clock: this would double-fire everything.
    for _ in 0..100 {
        host.run_fixed_for(&mut world, e.index(), dt, 0.5);
    }
    host.run(&mut world, &dir, dt, 0.5); // update() copies counters out
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let t = world.get::<Transform>(e).unwrap().translation;
    assert_eq!(t.x, 1.0, "after() must fire exactly once (got {})", t.x);
    assert_eq!(
        t.y, 5.0,
        "every(0.095) over 0.5s = 5 fires, cancelled timer never runs (got {})",
        t.y
    );
    let (final_alpha, tw_calls) = ((t.z as i32) / 1000, (t.z as i32) % 1000);
    assert_eq!(final_alpha, 1, "tween's final alpha must be exactly 1.0 (z = {})", t.z);
    assert!(
        (6..=8).contains(&tw_calls),
        "a 0.1s tween at 60Hz is ~7 per-tick calls, then stops (got {tw_calls})"
    );
}

#[test]
fn is_mine_and_find_scripts_pick_the_local_player() {
    // Two identical avatars, one probe: findScripts enumerates every
    // instance and net.isMine tells which one this machine controls —
    // how a shared camera finds the local player among many avatars.
    let dir = std::env::temp_dir().join("floptle_script_test_ismine");
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "avatar", "function update(node, dt) end\n");
    write_script(
        &dir,
        "probe",
        "function update(node, dt)\n\
           local list = findScripts(\"avatar\")\n\
           node.z = #list\n\
           for i, s in ipairs(list) do\n\
             if net.isMine(s.node) then node.x = i end\n\
           end\n\
           node.y = net.isMine(node) and 1 or 0\n\
         end\n",
    );
    let mut world = World::default();
    let avatar = |w: &mut World, x: f64| {
        let e = w.spawn();
        w.insert(
            e,
            Transform::from_translation(floptle_core::math::DVec3::new(x, 0.0, 0.0)),
        );
        w.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: "avatar".into(),
                enabled: true,
                params: vec![], refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
        e
    };
    let a1 = avatar(&mut world, 0.0);
    let a2 = avatar(&mut world, 10.0);
    let probe = world.spawn();
    world.insert(probe, Transform::IDENTITY);
    world.insert(
        probe,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "probe".into(),
            enabled: true,
            params: vec![], refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    let mut owners = HashMap::new();
    owners.insert(a1.index(), None); // networked, host-owned
    owners.insert(a2.index(), Some(2u64)); // networked, peer 2's avatar
    host.set_net_owners(owners);

    // On the server: the unowned avatar is mine; peer 2's is not.
    host.set_net_state(NetState {
        role: NetRoleState::Server,
        peers: vec![2],
        rtt_ms: 0.0,
        my_peer: None,
        ..Default::default()
    });
    host.run(&mut world, &dir, 0.016, 0.016);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let tr = world.get::<Transform>(probe).unwrap();
    assert_eq!(tr.translation.z, 2.0, "findScripts must list both avatars");
    assert_eq!(tr.translation.x, 1.0, "server: the unowned avatar is mine");
    assert_eq!(tr.translation.y, 1.0, "non-networked nodes are mine everywhere");

    // As client peer 2: only my own avatar is mine.
    host.set_net_state(NetState {
        role: NetRoleState::Client,
        peers: vec![],
        rtt_ms: 0.0,
        my_peer: Some(2),
        ..Default::default()
    });
    host.run(&mut world, &dir, 0.016, 0.032);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(
        world.get::<Transform>(probe).unwrap().translation.x,
        2.0,
        "client: peer 2 owns avatar 2"
    );
}

#[test]
fn net_rewind_swaps_poses_and_synced_vars_then_restores() {
    let dir = std::env::temp_dir().join("floptle_script_test_rewind");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "judge",
        "replicated = { parrying = false }\n\
         onRpc = {}\n\
         function onRpc.swing(args, sender)\n\
           net.rewind(sender, function()\n\
             local hit = raycast(0, 0, 0, 1, 0, 0, 50)\n\
             node.x = hit and hit.distance or -1\n\
             node.y = synced.parrying and 1 or 0\n\
           end)\n\
           local live = raycast(0, 0, 0, 1, 0, 0, 50)\n\
           node.z = live and live.distance or -1\n\
         end\n\
         function update(node, dt) end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "judge".into(),
            enabled: true,
            params: vec![], refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.set_net_state(NetState { role: NetRoleState::Server, peers: vec![7], rtt_ms: 0.0, my_peer: None, ..Default::default() });
    host.run(&mut world, &dir, 0.01, 0.01); // instantiate
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());

    // A target live at x = 10; the sender perceived it at x = 5, parrying.
    host.set_hulls(vec![hull(999, 10.0)]);
    host.set_rewind(Some(RewindScope {
        peer: 7,
        poses: vec![(999, [5.0, 0.0, 0.0])],
        synced: vec![(
            e.index(),
            "judge".into(),
            vec![("parrying".into(), floptle_net::NetValue::Bool(true))],
        )],
    }));
    host.dispatch_rpc(&mut world, "swing", &floptle_net::NetValue::Nil, 7);
    host.set_rewind(None);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let tr = world.get::<Transform>(e).unwrap();
    assert!(
        (tr.translation.x - 4.6).abs() < 0.05,
        "inside rewind the hull sits at the PERCEIVED x=5: {}",
        tr.translation.x
    );
    assert_eq!(tr.translation.y, 1.0, "synced.parrying reads the rewound tick's value");
    assert!(
        (tr.translation.z - 9.6).abs() < 0.05,
        "after rewind the live pose is back (x=10): {}",
        tr.translation.z
    );
    // The live synced store was restored too.
    let collected = host.collect_synced();
    assert_eq!(
        collected[0].2[0],
        ("parrying".to_string(), floptle_net::NetValue::Bool(false)),
        "rewind must not leak historical values into the present"
    );

    // Without a staged scope, rewind warns and runs at server time.
    host.drain_logs();
    host.dispatch_rpc(&mut world, "swing", &floptle_net::NetValue::Nil, 7);
    let tr = world.get::<Transform>(e).unwrap();
    assert!((tr.translation.x - 9.6).abs() < 0.05, "no scope ⇒ live pose");
    assert!(
        host.drain_logs().iter().any(|l| l.msg.contains("no lag-comp context")),
        "the fallback must be loud"
    );
}

/// The rollback contract: `snapshot()` captures, re-simulation mutates, and
/// `restore(s)` puts it back — with the engine owning the copy in both
/// directions, so a replay that mutates its restored state cannot corrupt the
/// snapshot it came from. That corruption is the failure mode that would only
/// show up under packet loss, on the second replay of the same tick.
#[test]
fn snapshot_and_restore_round_trip_and_survive_re_simulation() {
    let dir = std::env::temp_dir().join("floptle_script_test_rollback_hooks");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "fighter",
        "hp = 100\n\
         combo = { hits = 0, tags = { \"a\" } }\n\
         function fixedUpdate(node, dt)\n\
           hp = hp - 1\n\
           combo.hits = combo.hits + 1\n\
           table.insert(combo.tags, \"x\")\n\
         end\n\
         function snapshot() return { hp = hp, combo = combo } end\n\
         function restore(s) hp = s.hp; combo = s.combo end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
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
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0); // start()
    assert!(host.has_rollback_hooks(e.index()), "the hooks must be visible to the driver");

    let read = |h: &ScriptHost| -> (f64, f64, usize) {
        let env = h.instance_env(e.index(), "fighter").unwrap();
        let combo: mlua::Table = env.get("combo").unwrap();
        (
            env.get::<f64>("hp").unwrap(),
            combo.get::<f64>("hits").unwrap(),
            combo.get::<mlua::Table>("tags").unwrap().raw_len(),
        )
    };

    // Confirmed tick, then three provisional ones.
    let saved = host.snapshot_scripts(e.index());
    for _ in 0..3 {
        host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
    }
    assert_eq!(read(&host), (97.0, 3.0, 4));

    // A correction arrives: restore and re-simulate the same three ticks.
    host.restore_scripts(e.index(), &saved);
    assert_eq!(read(&host), (100.0, 0.0, 1), "restored to the confirmed tick");
    for _ in 0..3 {
        host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
    }
    assert_eq!(read(&host), (97.0, 3.0, 4), "the replay reproduces the same result");

    // …and the second replay off the same snapshot must too. It won't if the
    // capture shared its tables with the sim, because the first replay would
    // have mutated them.
    host.restore_scripts(e.index(), &saved);
    assert_eq!(read(&host), (100.0, 0.0, 1), "the snapshot is still pristine");
    for _ in 0..3 {
        host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
    }
    assert_eq!(read(&host), (97.0, 3.0, 4));
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
}

/// `net.random()` (`docs/multiplayer.md` §3): identical on
/// every peer for a tick, identical again when that tick is re-simulated.
///
/// The second half is the one a hand-rolled `rng(matchSeed + tick)` gets
/// wrong — it re-seeds per tick but not per *draw*, so two calls in one tick
/// return the same number, and authors work around that by adding state
/// that then has to be rolled back too.
#[test]
fn net_random_is_identical_per_tick_and_across_a_replay() {
    let dir = std::env::temp_dir().join("floptle_script_test_net_random");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "roller",
        "rolls = {}\n\
         function fixedUpdate(node, dt)\n\
           rolls[#rolls + 1] = net.random()\n\
           rolls[#rolls + 1] = net.random()\n\
           rolls[#rolls + 1] = net.random(1, 6)\n\
         end\n\
         function snapshot() return { n = #rolls } end\n\
         function restore(s) while #rolls > s.n do table.remove(rolls) end end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "roller".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    let info = |tick: u64| RollbackInfo {
        active: true,
        tick,
        seed: 0x0BAD_F00D_1234_5678,
        ..Default::default()
    };
    let read = |h: &ScriptHost| -> Vec<f64> {
        h.instance_env(e.index(), "roller")
            .unwrap()
            .get::<mlua::Table>("rolls")
            .unwrap()
            .sequence_values::<f64>()
            .flatten()
            .collect()
    };

    for tick in 1..=3u64 {
        host.set_rollback_info(info(tick));
        host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
    }
    let live = read(&host);
    assert_eq!(live.len(), 9);
    assert_ne!(live[0], live[1], "two draws in one tick must differ");
    assert_ne!(live[0], live[3], "and two ticks must differ");
    assert!(live.iter().take(2).all(|v| (0.0..1.0).contains(v)), "unit range");
    assert!(live[2] >= 1.0 && live[2] <= 6.0 && live[2].fract() == 0.0, "a d6: {}", live[2]);

    // Re-simulate ticks 2..=3 — as a correction would. The script keeps
    // appending, so the replay's draws land after the live ones and the two
    // stretches can be compared directly.
    for tick in 2..=3u64 {
        host.set_rollback_info(info(tick));
        host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
    }
    let replayed = read(&host);
    assert_eq!(&replayed[9..], &live[3..], "a replayed tick must roll the same numbers");
}

/// The `replaying` gate (`docs/multiplayer.md` §4): a
/// re-simulated tick runs the same Lua the live tick ran, so its one-shot
/// cosmetics must not fire a second time — while everything the simulation
/// depends on still lands, and a raised error still reaches the Console.
#[test]
fn a_replay_suppresses_one_shot_side_effects_but_not_simulation_writes() {
    let dir = std::env::temp_dir().join("floptle_script_test_replay_gate");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "noisy",
        "hits = 0\n\
         function fixedUpdate(node, dt)\n\
           hits = hits + 1\n\
           node.vx = hits\n\
           print(\"hit \" .. hits)\n\
           spawnEffect(\"spark\", 1, 2, 3)\n\
           audio.play(\"thud\")\n\
           spawn(\"fireball\", vec3(0, 0, 0))\n\
           net.rpc(\"scored\", { n = hits })\n\
         end\n\
         function snapshot() return { hits = hits } end\n\
         function restore(s) hits = s.hits end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "noisy".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    let saved = host.snapshot_scripts(e.index());

    // Two live ticks: every queue fills as usual.
    for _ in 0..2 {
        host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
    }
    assert_eq!(host.take_spawn_effects().len(), 2);
    assert_eq!(host.take_audio_commands().len(), 2);
    assert_eq!(host.take_spawn_requests().len(), 2);
    assert_eq!(host.take_net_commands().len(), 2);
    assert_eq!(host.drain_logs().len(), 2);
    assert_eq!(host.take_body_changes().get(&e.index()).map(|v| v[0]), Some(2.0));

    // A correction: the same two ticks re-simulate under the gate.
    host.restore_scripts(e.index(), &saved);
    host.begin_replay();
    assert!(host.is_replaying());
    for _ in 0..2 {
        host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
    }
    host.end_replay();
    assert!(!host.is_replaying());

    assert!(host.take_spawn_effects().is_empty(), "the hit spark must not double");
    assert!(host.take_audio_commands().is_empty(), "nor the impact stutter");
    assert!(host.take_spawn_requests().is_empty(), "nor the projectile duplicate");
    assert!(host.take_net_commands().is_empty(), "nor the rpc send twice");
    assert!(host.drain_logs().is_empty(), "nor the Console flood");
    // …while the simulation write the replay exists to produce still lands.
    assert_eq!(
        host.take_body_changes().get(&e.index()).map(|v| v[0]),
        Some(2.0),
        "body writes are the POINT of the replay and must survive the gate"
    );
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
}

/// A replay that throws is a correctness problem, not noise: suppressing it
/// would leave a desync with no symptom at all.
#[test]
fn a_replay_never_suppresses_an_error() {
    let dir = std::env::temp_dir().join("floptle_script_test_replay_errors");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "thrower",
        "function fixedUpdate(node, dt) print(\"quiet\"); error(\"boom\") end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "thrower".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    host.begin_replay();
    host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
    host.end_replay();
    let logs = host.drain_logs();
    assert!(
        logs.iter().any(|l| l.level == LogLevel::Error && l.msg.contains("boom")),
        "the raised error must survive the gate: {logs:?}"
    );
    // The print's own line must not be there. Matched with the level as
    // well as the text, because a runtime error now quotes the source line
    // it happened on (`crate::runtime_error`) — and in this script the
    // print and the `error()` share one line, so the error's own log
    // legitimately contains the word "quiet". Checking the text alone would
    // be asserting that the error message says less than it does.
    assert!(
        !logs.iter().any(|l| l.level != LogLevel::Error && l.msg.contains("quiet")),
        "…but the print must not: {logs:?}"
    );
}
