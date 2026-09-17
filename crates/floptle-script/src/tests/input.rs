use super::*;

/// Polling a key the host keeps says so, once, instead of reading `false`
/// forever.
///
/// The failure this replaces has no symptom: `input.pressed(k)` returning
/// false is what a key nobody pressed also looks like, so there is nothing
/// to log, nothing to assert and nothing to fall back to from inside the
/// game. One Console line is the whole difference between a five-second fix
/// and a player telling you your feature does not exist.
#[test]
fn polling_a_reserved_key_warns_once_and_says_what_takes_it() {
    let dir = std::env::temp_dir().join(format!("floptle_reserved_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "hotkey",
        "\
function update(node, dt)
  if input.pressed('f1') then end
  if input.key('F1') then end      -- same key, different spelling
  if input.pressed('i') then end   -- a key the game actually gets
end
",
    );
    let (mut world, _e) = world_with_script("hotkey");
    let mut host = ScriptHost::new();
    host.set_reserved_keys(&[("f1", "Play / Stop")]);
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    let warns: Vec<String> = host
        .drain_logs()
        .into_iter()
        .filter(|l| l.level == LogLevel::Warn)
        .map(|l| l.msg)
        .collect();
    assert_eq!(warns.len(), 1, "one line per key, not one per poll: {warns:?}");
    assert!(warns[0].contains("f1"), "names the key: {}", warns[0]);
    assert!(warns[0].contains("Play / Stop"), "names what takes it: {}", warns[0]);

    // …and it does not repeat every frame. A warning that floods the Console
    // is a warning that gets scrolled past.
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    assert!(
        host.drain_logs().iter().all(|l| l.level != LogLevel::Warn),
        "the same key warned twice"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// With nothing reserved — a headless harness, or a build that keeps no keys
/// — the check is silent. The default must not invent warnings.
#[test]
fn nothing_is_reserved_by_default() {
    let dir = std::env::temp_dir().join(format!("floptle_unreserved_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "poll", "function update(node, dt)\n  if input.pressed('f1') then end\nend\n");
    let (mut world, _e) = world_with_script("poll");
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.drain_logs().iter().all(|l| l.level != LogLevel::Warn));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn input_api_drives_a_script() {
    let dir = std::env::temp_dir().join("floptle_script_test_input");
    let _ = std::fs::create_dir_all(&dir);
    // Move +z while "w" is held; jump (+y) on the click edge.
    write_script(
        &dir,
        "mover",
        "function update(node, dt)\n  if input.key('w') then node.z = node.z + 1.0 end\n  if input.clicked(0) then node.y = node.y + 5.0 end\nend\n",
    );
    let (mut world, e) = world_with_script("mover");
    let mut host = ScriptHost::new();

    // No input → no movement.
    host.run(&mut world, &dir, 0.1, 0.1);
    assert_eq!(world.get::<Transform>(e).unwrap().translation.z, 0.0);

    // Hold "w" + click → moves +z and jumps +y.
    let mut snap = InputSnapshot::default();
    snap.keys_down.insert("w".into());
    snap.buttons_pressed[0] = true;
    host.set_input(snap);
    host.run(&mut world, &dir, 0.1, 0.1);
    let t = world.get::<Transform>(e).unwrap();
    assert!(t.translation.z >= 1.0, "w should move +z, z={}", t.translation.z);
    assert!(t.translation.y >= 5.0, "click should jump +y, y={}", t.translation.y);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
}

#[test]
fn input_released_edge() {
    let dir = std::env::temp_dir().join("floptle_script_test_released");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "rel",
        "function update(node, dt)\n  if input.released('e') then node.x = node.x + 1 end\nend\n",
    );
    let (mut world, e) = world_with_script("rel");
    let mut host = ScriptHost::new();
    // Release edge → +1.
    let mut snap = InputSnapshot::default();
    snap.keys_released.insert("e".into());
    host.set_input(snap);
    host.run(&mut world, &dir, 0.1, 0.0);
    assert!((world.get::<Transform>(e).unwrap().translation.x - 1.0).abs() < 1e-6);
    // No release → unchanged.
    host.set_input(InputSnapshot::default());
    host.run(&mut world, &dir, 0.1, 0.0);
    assert!((world.get::<Transform>(e).unwrap().translation.x - 1.0).abs() < 1e-6);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
}
