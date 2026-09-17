use super::*;

/// The 2D layer, end to end from Lua: build a grid, paint
/// squares, read them back, and draw sprites into a batch.
#[test]
fn a_script_builds_a_tilemap_and_fills_a_sprite_batch() {
    let dir = std::env::temp_dir().join(format!("floptle_2d_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "flat",
        "\
function start(node)
  node:setTilemap{ cols = 4, rows = 3, tile = 2.0 }
end

function update(node, dt)
  local tm = node:tilemap()
  tm:fill(1)
  tm:set(0, 0, 7)
  tm:set(3, 2, 9)
  tm:set(99, 99, 5)      -- outside the grid: a no-op, not a wrap
  readBack = tm:get(0, 0)
  cols, rows = tm:size()

  -- A node is not a sprite batch until it is told to be one, and taking the
  -- handle in the very next line has to work. A separate node
  -- because Matter is exclusive: a tilemap is not also a batch.
  local nd = find('Batch')
  nd:setSpriteBatch{ size = 1.0 }
  local b = nd:sprites()
  b:draw(1, 2)                                   -- the short form
  b:draw(3, 4, 0, 2.0, 1.5, 6, 1, 0.2, 0.2, 0.5) -- …and the whole thing
  b:draw(5, 6, 0, vec2(1.4, 0.6))                -- squash and stretch
end
",
    );
    let (mut world, e) = world_with_script("flat");
    world.insert(e, floptle_core::Matter::Empty);
    let batch = world.spawn();
    world.insert(batch, Transform::IDENTITY);
    world.insert(batch, floptle_core::Name("Batch".into()));
    world.insert(batch, floptle_core::Matter::Empty);
    let mut host = ScriptHost::new();
    // Two passes: `start` builds the grid, and the writes queued in the
    // first `update` land before the second reads them back.
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());

    let Some(floptle_core::Matter::Tilemap { cols, rows, tile, data, .. }) =
        world.get::<floptle_core::Matter>(e)
    else {
        panic!("setTilemap did not make a tilemap: {:?}", world.get::<floptle_core::Matter>(e))
    };
    assert_eq!((*cols, *rows, *tile), (4, 3, 2.0));
    assert_eq!(data.len(), 12, "the grid is sized even with no data given");
    assert_eq!(data[0], 7, "tm:set(0, 0, ..) writes the top-left");
    assert_eq!(data[11], 9, "…and (3, 2) the bottom-right");
    assert_eq!(data[1], 1, "tm:fill covered the rest");
    assert!(data.iter().all(|c| *c != 5), "an out-of-bounds set must not wrap");

    // `setSpriteBatch` made the other node a batch, from Lua alone.
    assert!(
        matches!(
            world.get::<floptle_core::Matter>(batch),
            Some(floptle_core::Matter::SpriteBatch { .. })
        ),
        "setSpriteBatch made it a batch: {:?}",
        world.get::<floptle_core::Matter>(batch)
    );
    host.run(&mut world, &dir, 1.0 / 60.0, 2.0 / 60.0);
    let sprites = world.get::<floptle_core::Sprites>(batch).expect("sprites");
    assert_eq!(sprites.0.len(), 3, "three draws, three sprites");
    assert_eq!(sprites.0[0].pos, [1.0, 2.0, 0.0]);
    assert_eq!(sprites.0[0].tint, [1.0, 1.0, 1.0, 1.0], "the short form is untinted");
    assert_eq!(sprites.0[0].scale, [1.0, 1.0], "…and unscaled");
    assert_eq!(sprites.0[1].cell, 6);
    assert_eq!(sprites.0[1].tint, [1.0, 0.2, 0.2, 0.5], "the per-sprite tint survives");
    assert_eq!(sprites.0[1].scale, [2.0, 2.0], "one number scales both axes");
    assert_eq!(sprites.0[2].scale, [1.4, 0.6], "…and a vec2 stretches one of them");

    // IMMEDIATE mode: a pass that draws nothing leaves nothing behind.
    write_script(&dir, "flat", "function update(node, dt)\nend\n");
    host.run(&mut world, &dir, 1.0 / 60.0, 3.0 / 60.0);
    assert!(
        world.get::<floptle_core::Sprites>(batch).is_some_and(|s| s.0.is_empty()),
        "sprites must not survive a frame nobody drew them"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The 2D surface a real tilemap game reaches for, end to end through Lua.
///
/// Every one of these was hand-rolled in both in-house games before it
/// existed, and each hand-rolled copy was wrong in the same way: it
/// duplicated the grid's centring and its row-0-is-the-top convention, and
/// went stale the moment the map was moved. So the test that matters is not
/// "does `set` write a square" — it is "does the world conversion survive the
/// node's transform", which is the part a script cannot check for itself.
#[test]
fn a_script_can_place_read_and_locate_tiles_through_the_handle() {
    let dir = std::env::temp_dir().join(format!("floptle_tiles2d_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "level",
        "\
function start(node)
  node:setTilemap{ cols = 4, rows = 3, tile = 2.0 }
  local tm = node:tilemap()
  tm:fill(0)
  -- A turned tile: rot is degrees clockwise, and the pair reads back canonically.
  tm:set(1, 1, 5, { rot = 90 })
  tm:set(2, 1, 6, { flipX = true })
  -- A rectangle, corners in either order, clipped at the edge.
  tm:fillRect(3, 0, 9, 9, 7)
end

-- The reads live in `update`, because the construction API is DEFERRED: what
-- `start` queued lands in the flush after it, and the scene mirror a handle
-- reads is rebuilt at the top of the next pass. A game reading back its own
-- writes in the same hook is reading the frame before.
function update(node, dt)
  local tm = node:tilemap()
  cols, rows = tm:size()
  edge = tm:tileSize()
  cellAt11, xf11, flip11 = tm:at(1, 1)
  plainGet = tm:get(1, 1)
  -- World <-> cell, through the node's own transform.
  local c = tm:worldAt(0, 0)
  cx, cy = tm:cellAt(c)
  -- …and a point well outside the map is off the map, not clamped to an edge.
  offX, offY = tm:cellAt(vec3(1000, 0, 0))
  clipped = tm:get(3, 2)
end
",
    );
    let (mut world, e) = world_with_script("level");
    world.insert(e, floptle_core::Matter::Empty);
    // A moved, TURNED and SCALED map — the case a Lua copy of the maths gets
    // wrong. If `cellAt(worldAt(0, 0))` still comes back (0, 0) here, the
    // conversion is going through the transform rather than assuming
    // identity.
    world.insert(
        e,
        Transform {
            translation: glam::DVec3::new(37.0, -12.0, 4.0),
            rotation: glam::Quat::from_rotation_z(0.7),
            scale: glam::Vec3::new(1.5, 1.5, 1.0),
        },
    );
    let mut host = ScriptHost::new();
    // Two passes: `start` queues the construction writes, and the second run
    // re-mirrors the scene so the reads see them.
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());

    let env = host.instance_env(e.index(), "level").expect("the script ran");
    let num = |k: &str| env.get::<f64>(k).unwrap_or(-999.0);
    assert_eq!((num("cols"), num("rows")), (4.0, 3.0));
    assert_eq!(num("edge"), 2.0, "tm:tileSize is the world edge of one square");

    assert_eq!(num("cellAt11"), 5.0, "tm:at gives the cell");
    assert_eq!(num("xf11"), 90.0, "…and the rotation, in degrees clockwise");
    assert_eq!(num("plainGet"), 5.0, "tm:get strips the orientation");
    assert!(
        !env.get::<bool>("flip11").unwrap_or(true),
        "a pure rotation is not mirrored"
    );

    assert_eq!(
        (num("cx"), num("cy")),
        (0.0, 0.0),
        "worldAt then cellAt must round-trip THROUGH the node's transform"
    );
    assert!(
        env.get::<mlua::Value>("offX").map(|v| v.is_nil()).unwrap_or(false),
        "a point off the map is nil, not clamped to an edge square"
    );
    assert_eq!(num("clipped"), 7.0, "the rectangle clipped to the grid and filled the corner");

    // The component itself carries the packed orientation, so a saved scene
    // records it and the mesh draws it.
    let Some(floptle_core::Matter::Tilemap { data, .. }) =
        world.get::<floptle_core::Matter>(e)
    else {
        panic!("setTilemap did not make a tilemap")
    };
    // row 1, column 1 of a 4-wide grid.
    let turned = data[4 + 1];
    assert_eq!(floptle_core::tile_index(turned), 5);
    assert_eq!(floptle_core::tile_xform(turned), floptle_core::TileXform::new(1, false));
    let mirrored = data[4 + 2];
    assert_eq!(floptle_core::tile_index(mirrored), 6);
    assert!(floptle_core::tile_xform(mirrored).flip_x, "flipX = true must mirror it");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A wrong orientation is refused where it was written, not rounded down to
/// something that looks almost right.
#[test]
fn a_bad_tile_orientation_is_refused_at_the_call() {
    let dir = std::env::temp_dir().join(format!("floptle_tilexf_bad_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let cases: &[(&str, &[&str])] = &[
        // 45 degrees is not one of the eight things a square tile can be.
        ("tm:set(0, 0, 1, { rot = 45 })", &["rot = 45", "quarter-turns"]),
        // A typo in an orientation key is a tile placed unturned, silently.
        ("tm:set(0, 0, 1, { flipx = true })", &["flipx", "did you mean `flipX`"]),
        // A resize with nothing to resize to is a mistake, not a no-op.
        ("tm:resize{}", &["cols", "rows"]),
        ("tm:resize{ colls = 4 }", &["colls", "did you mean `cols`"]),
    ];
    for (i, (src, wants)) in cases.iter().enumerate() {
        let name = format!("tbad{i}");
        write_script(
            &dir,
            &name,
            &format!(
                "function start(node)\n  node:setTilemap{{ cols = 2, rows = 2 }}\n  \
                 local tm = node:tilemap()\n  {src}\nend\n"
            ),
        );
        let (mut world, e) = world_with_script(&name);
        world.insert(e, floptle_core::Matter::Empty);
        let mut host = ScriptHost::new();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        let errs = host.errors().to_vec();
        assert!(!errs.is_empty(), "`{src}` was accepted silently");
        let msg = errs.join(" | ");
        for want in *wants {
            assert!(msg.contains(want), "`{src}` error is missing {want:?}: {msg}");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// The arena loop that shipped with its walls missing now runs to the end.
///
/// This is the real thing, not a unit test of the converter: a wall tilemap
/// written row by row with the play area punched out of the middle, and a
/// line after the loop that has to be reached. `-1` used to fail the `u32`
/// conversion and raise, so the loop died on the first inside square — two
/// rows in — the mesh kept its padding, and the node never reached the line
/// that positions it. What the player saw was "the walls are not visible".
///
/// The `EMPTY_TILE` global is checked in the same pass because it is what
/// the editor's autocomplete has told people to write since tilemaps
/// shipped; before this it resolved to `nil`, which then also failed to
/// convert.
/// the mirror now REUSES a tilemap's buffer instead of
/// reallocating it every sync. The whole risk in that is staleness — a map
/// that changed must still read as changed, on the very next frame — so this
/// writes through the handle, steps frames, and reads back.
#[test]
fn a_reused_tilemap_mirror_still_sees_the_map_change() {
    let dir = std::env::temp_dir().join(format!("floptle_tm_reuse_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "tick",
        "\
function start(node)
  node:setTilemap{ cols = 4, rows = 4, tile = 1.0 }
  frame = 0
  stale = 0
end
function update(node, dt)
  local tm = node:tilemap()
  local got = tm:get(1, 1)
  -- The very first update runs before setTilemap has been applied, so there is
  -- nothing to read yet. From then on, what we read must be exactly what we
  -- wrote last frame — a reused buffer that was not refreshed would hand back
  -- an older number.
  if frame > 0 and got ~= frame then stale = stale + 1 end
  frame = frame + 1
  tm:set(1, 1, frame)
  tm:set(0, 0, stale)
end
",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "tick".into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    for _ in 0..4 {
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    }
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let Some(Matter::Tilemap { data, .. }) = world.get::<Matter>(e) else {
        panic!("no tilemap")
    };
    assert_eq!(
        floptle_core::tile_index(data[0]),
        0,
        "a frame read a stale grid — the reused buffer was not refreshed"
    );
    // Four runs, the first of which only creates the map: three writes land.
    assert_eq!(
        floptle_core::tile_index(data[5]),
        4,
        "the last write did not reach the ECS"
    );
}

#[test]
fn punching_a_hole_in_a_wall_tilemap_runs_to_the_end_of_the_loop() {
    let dir = std::env::temp_dir().join(format!("floptle_empty_tile_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "arena",
        "\
function start(node)
  local gw, gh, band = 5, 5, 1
  node:setTilemap{ cols = gw, rows = gh, tile = 1.0 }
  local tm = node:tilemap()
  for gy = 0, gh - 1 do
for gx = 0, gw - 1 do
  local inside = gx >= band and gx < gw - band and gy >= band and gy < gh - band
  tm:set(gx, gy, inside and -1 or 4)
end
  end
  -- The three other spellings of empty all have to reach the same value.
  tm:set(2, 2, EMPTY_TILE)
  tm:set(3, 2, tm.EMPTY)
  tm:set(1, 3, nil)
  -- The line after the loop. This is the one the raise used to eat.
  reachedTheEnd = true
end
",
    );
    let (mut world, e) = world_with_script("arena");
    world.insert(e, floptle_core::Matter::Empty);
    let mut host = ScriptHost::new();
    // `start` queues the grid and the writes; the second pass applies them.
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    assert!(host.errors().is_empty(), "a negative cell must not raise: {:?}", host.errors());

    let Some(floptle_core::Matter::Tilemap { data, .. }) =
        world.get::<floptle_core::Matter>(e)
    else {
        panic!("no tilemap")
    };
    // The border is wall on every side — the loop got all the way round,
    // rather than dying on the first square that wanted to be empty.
    for (i, cell) in data.iter().enumerate() {
        let (x, y) = (i as u32 % 5, i as u32 / 5);
        let border = x == 0 || y == 0 || x == 4 || y == 4;
        let want = if border { 4 } else { floptle_core::EMPTY_TILE };
        assert_eq!(*cell, want, "cell ({x}, {y}) is wrong: the play area is the empty part");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// A sprite survives to the end of the frame whichever pass drew it.
///
/// The batches used to be emptied after every pass, so the fixed pass wiped
/// whatever `update` drew and the late pass wiped that — leaving `lateUpdate`
/// as the only place a draw survived, silently, with no error and nothing on
/// screen. `update` is where every tutorial puts per-frame work and where
/// `draw.*` goes, so the one obvious spelling was the one that could not work.
#[test]
fn a_sprite_drawn_in_any_pass_survives_the_frame() {
    let dir = std::env::temp_dir().join(format!("floptle_0070_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "render",
        "\
frames = 0
function start(node)
  node:setSpriteBatch{ size = 1.0 }
  find('Fixed'):setSpriteBatch{ size = 1.0 }
  find('Late'):setSpriteBatch{ size = 1.0 }
end

-- One batch per pass, so a wipe by a LATER pass is visible as an empty list
-- rather than hidden by the next pass redrawing the same thing.
function update(node, dt)
  frames = frames + 1
  if frames > 2 then return end     -- …and then the game stops drawing entirely
  node:sprites():draw(1, 1)
  node:sprites():draw(2, 2)
end

function fixedUpdate(node, dt)
  if frames > 2 then return end
  find('Fixed'):sprites():draw(3, 3)
end

function lateUpdate(node, dt)
  if frames > 2 then return end
  find('Late'):sprites():draw(4, 4)
end
",
    );
    let (mut world, e) = world_with_script("render");
    world.insert(e, floptle_core::Name("Frame".into()));
    world.insert(e, floptle_core::Matter::Empty);
    let mut named = |n: &str| {
        let b = world.spawn();
        world.insert(b, Transform::IDENTITY);
        world.insert(b, floptle_core::Name(n.into()));
        world.insert(b, floptle_core::Matter::Empty);
        b
    };
    let fixed = named("Fixed");
    let late = named("Late");

    let mut host = ScriptHost::new();
    let count = |world: &World, b| {
        world.get::<floptle_core::Sprites>(b).map(|s| s.0.len()).unwrap_or(0)
    };
    // The driver's whole frame, in its real order.
    for f in 0..2 {
        let t = f as f32 / 60.0;
        host.run(&mut world, &dir, 1.0 / 60.0, t);
        host.run_fixed(&mut world, 1.0 / 60.0, t);
        host.run_late(&mut world, 1.0 / 60.0, t);
        assert!(host.errors().is_empty(), "{:?}", host.errors());

        assert_eq!(count(&world, e), 2, "frame {f}: the `update` draws survived to the end");
        assert_eq!(count(&world, fixed), 1, "frame {f}: so did the `fixedUpdate` draw");
        assert_eq!(count(&world, late), 1, "frame {f}: and the `lateUpdate` draw");
    }

    // Still immediate mode: the frame is the unit, so a frame nobody draws
    // in clears every batch — no pool to grow, nothing to `clear()`.
    host.run(&mut world, &dir, 1.0 / 60.0, 2.0 / 60.0);
    host.run_fixed(&mut world, 1.0 / 60.0, 2.0 / 60.0);
    host.run_late(&mut world, 1.0 / 60.0, 2.0 / 60.0);
    for (b, who) in [(e, "update"), (fixed, "fixedUpdate"), (late, "lateUpdate")] {
        assert_eq!(count(&world, b), 0, "a frame with no draws empties the {who} batch");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// `node:sprites()` on a node that is not a batch used to return a handle
/// whose every draw was collected and then dropped by the renderer's own
/// filter — no error, no warning, nothing drawn, ever.
#[test]
fn asking_a_plain_node_for_a_sprite_batch_says_so() {
    let dir = std::env::temp_dir().join(format!("floptle_0062_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "flat",
        "function update(node, dt)\n  local b = node:sprites()\n  b:draw(1, 2)\nend\n",
    );
    let (mut world, e) = world_with_script("flat");
    world.insert(e, floptle_core::Matter::Empty);
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    let errs = host.errors().to_vec();
    assert_eq!(errs.len(), 1, "it must complain: {errs:?}");
    assert!(
        errs[0].contains("setSpriteBatch"),
        "…and name the call that fixes it: {}",
        errs[0]
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn particles_api_queues_commands_and_reads_state() {
    let dir = std::env::temp_dir().join("floptle_script_test_vfx");
    let _ = std::fs::create_dir_all(&dir);
    // First frame: not playing → play(). Once the editor reports it playing, read
    // alive() into node.y.
    write_script(
        &dir,
        "smoke",
        "function update(node, dt)\n  local p = node:particles()\n  if p:isPlaying() then node.y = p:alive() else p:play() end\nend\n",
    );
    let (mut world, e) = world_with_script("smoke");
    world.insert(e, ParticleSystem { asset: "vfx/Smoke".into(), play_on_start: false });
    let mut host = ScriptHost::new();

    // Frame 1: empty info → isPlaying() false → the script queues play().
    host.run(&mut world, &dir, 0.1, 0.1);
    let cmds = host.take_vfx_commands();
    assert_eq!(cmds.len(), 1, "play() must queue exactly one command");
    assert!(matches!(cmds[0], (idx, VfxCmd::Play) if idx == e.index()), "wrong cmd: {cmds:?}");

    // Frame 2: the editor reports it playing with 12 alive → the script reads alive().
    host.set_vfx_info(HashMap::from([(
        e.index(),
        VfxInfo { playing: true, alive: 12, asset: "vfx/Smoke".into() },
    )]));
    host.run(&mut world, &dir, 0.1, 0.1);
    assert_eq!(
        world.get::<Transform>(e).unwrap().translation.y,
        12.0,
        "alive() must read the fed count"
    );
    assert!(host.take_vfx_commands().is_empty(), "no play() when already playing");
}

/// Ground truth for the `cond and X or Y` conditional idiom through the real
/// host — with animator METHOD calls in the chain — plus the animator getters
/// reading the fed mirror. Lua's ternary spelling is core syntax; the reported
/// "errors writing statements like that" came from method casing (see
/// `animator_method_typo_names_the_camel_case_fix`), not from the idiom.
#[test]
fn animator_getters_and_conditional_idiom() {
    let dir = std::env::temp_dir().join("floptle_script_test_anim");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "locomotion",
        "function update(node, dt)\n  local anim = node:animator()\n  node.x = anim:isPlaying('Running') and 2 or anim:isPlaying('Walking') and 1 or 0\n  node.y = anim:time() or -1\n  node.z = ((anim:current() == 'Running') and 10 or 0) + #anim:clips()\nend\n",
    );
    let (mut world, e) = world_with_script("locomotion");
    let mut host = ScriptHost::new();

    // The IDE's red-squiggle path must accept the idiom too.
    assert!(
        host.check_syntax(
            "function f(a) return a:isPlaying('R') and 2 or a:isPlaying('W') and 1 or 0 end"
        )
        .is_none(),
        "and/or chain must parse cleanly"
    );

    let info = |state: &str, t: f32, fin: bool| {
        HashMap::from([(
            e.index(),
            AnimInfo {
                layers: vec![("Base".into(), Some(state.into()), t, fin)],
                clips: Rc::new(
                    ["Idle", "Walking", "Running"]
                        .iter()
                        .map(|n| ClipInfo {
                            name: (*n).into(),
                            duration: 1.0,
                            events: Vec::new(),
                        })
                        .collect(),
                ),
            },
        )])
    };
    let pos = |world: &World| world.get::<Transform>(e).unwrap().translation;

    // Running → the chain picks 2; current()/time()/clips() read the mirror.
    host.set_anim_info(info("Running", 0.25, false));
    host.run(&mut world, &dir, 0.1, 0.1);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let p = pos(&world);
    assert_eq!(p.x, 2.0, "isPlaying('Running') and 2 must win");
    assert!((p.y - 0.25).abs() < 1e-5, "time() reads the fed playhead");
    assert_eq!(p.z, 13.0, "current()=='Running' (10) + 3 clips");

    // Walking → the middle arm.
    host.set_anim_info(info("Walking", 1.5, false));
    host.run(&mut world, &dir, 0.1, 0.2);
    assert_eq!(pos(&world).x, 1.0, "isPlaying('Walking') and 1 must win");

    // Running but FINISHED → isPlaying is false → the chain falls to 0.
    host.set_anim_info(info("Running", 2.0, true));
    host.run(&mut world, &dir, 0.1, 0.3);
    assert_eq!(pos(&world).x, 0.0, "a finished one-shot is not 'playing'");

    // No animator mirror at all → every arm false → 0 (and no errors).
    host.set_anim_info(HashMap::new());
    host.run(&mut world, &dir, 0.1, 0.4);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(pos(&world).x, 0.0);
}

/// A CASING typo on an animator method (`anim:IsPlaying`) must fail with a
/// did-you-mean naming the camelCase method — not a bare "attempt to call a
/// nil value". Genuinely unknown keys still index to nil (feature probes).
#[test]
fn animator_method_typo_names_the_camel_case_fix() {
    let dir = std::env::temp_dir().join("floptle_script_test_anim_typo");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "typo",
        "function update(node, dt)\n  if dt < 0.15 then\n    if node:animator().notAThing == nil then node.y = 7 end\n  else\n    node.x = node:animator():IsPlaying('Run') and 2 or 0\n  end\nend\n",
    );
    let (mut world, e) = world_with_script("typo");
    let mut host = ScriptHost::new();
    // Frame 1: the unknown-key probe indexes to nil — no error, the write lands.
    host.run(&mut world, &dir, 0.1, 0.1);
    assert!(host.errors().is_empty(), "nil probe must not error: {:?}", host.errors());
    assert_eq!(world.get::<Transform>(e).unwrap().translation.y, 7.0);
    // Frame 2: the casing typo errors with the camelCase suggestion.
    host.run(&mut world, &dir, 0.2, 0.3);
    let errs = host.errors().join("\n");
    assert!(
        errs.contains("did you mean 'isPlaying'"),
        "typo must suggest the camelCase method: {errs}"
    );
}

#[test]
fn audio_play_queues_and_handle_controls() {
    let dir = std::env::temp_dir().join("floptle_script_test_audio");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "sfx",
        "function update(node, dt)\n  local s = audio.play('audio/hit.ogg', 1.0, 2.0, 3.0, { maxDistance = 35, track = 'SFX', endBehavior = 'Destroy' })\n  s:setVolume(0.5)\n  audio.play('audio/music.ogg', { loop = true })\n  audio.track('Music'):setVolume(-6)\nend\n",
    );
    let (mut world, _e) = world_with_script("sfx");
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);
    let cmds = host.take_audio_commands();
    assert_eq!(cmds.len(), 4, "expected play+setVolume+play+trackVolume: {cmds:?}");
    let AudioCmd::Play { handle, clip, at, params } = &cmds[0] else {
        panic!("first cmd must be Play: {cmds:?}")
    };
    assert_eq!(clip, "audio/hit.ogg");
    assert!(matches!(at, AudioAt::Pos([1.0, 2.0, 3.0])), "positional play: {at:?}");
    assert_eq!(params.max_distance, 35.0);
    assert_eq!(params.track, "SFX");
    assert_eq!(params.end, floptle_audio::EndBehavior::Destroy);
    assert!(
        matches!(&cmds[1], AudioCmd::SetParam { handle: h, field, value }
            if h == handle && field == "volume" && *value == 0.5),
        "handle setter must target the played sound: {cmds:?}"
    );
    let AudioCmd::Play { at: at2, params: p2, .. } = &cmds[2] else {
        panic!("third cmd must be the flat play: {cmds:?}")
    };
    assert!(matches!(at2, AudioAt::Flat), "opts-only play is flat: {at2:?}");
    assert_eq!(p2.end, floptle_audio::EndBehavior::Loop, "loop = true shorthand");
    assert!(
        matches!(&cmds[3], AudioCmd::TrackVolume { track, db } if track == "Music" && *db == -6.0),
        "mixer track handle: {cmds:?}"
    );
    assert!(host.take_audio_commands().is_empty(), "drained");
}

#[test]
fn node_sound_handle_and_component_mirror() {
    let dir = std::env::temp_dir().join("floptle_script_test_audio_src");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "alarm",
        "function update(node, dt)\n  if not node:sound():isPlaying() then node:sound():play() end\n  node:getcomponent('AudioSource').volume = 0.25\nend\n",
    );
    let (mut world, e) = world_with_script("alarm");
    world.insert(e, floptle_audio::AudioSource { clip: "audio/alarm.ogg".into(), ..Default::default() });
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);
    let cmds = host.take_audio_commands();
    assert!(
        matches!(cmds.as_slice(), [AudioCmd::SourcePlay { ent }] if *ent == e.index()),
        "not playing -> one SourcePlay: {cmds:?}"
    );
    assert_eq!(
        world.get::<floptle_audio::AudioSource>(e).unwrap().params.volume,
        0.25,
        "component mirror write must land on the ECS"
    );

    // Once the mirror says it's playing, no more play commands.
    let mut info = AudioInfo::default();
    info.sources.insert(
        e.index(),
        AudioPlayState { playing: true, paused: false, position: 0.5 },
    );
    host.set_audio_info(info);
    host.run(&mut world, &dir, 0.1, 0.1);
    assert!(host.take_audio_commands().is_empty(), "no play() when already playing");
}

#[test]
fn spawn_effect_global_queues_a_one_shot() {
    let dir = std::env::temp_dir().join("floptle_script_test_spawnfx");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "boom",
        "function update(node, dt)\n  spawnEffect('vfx/Impact', 1.0, 2.0, 3.0)\nend\n",
    );
    let (mut world, _e) = world_with_script("boom");
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);
    let spawns = host.take_spawn_effects();
    assert_eq!(spawns.len(), 1, "one spawnEffect call = one queued one-shot");
    assert_eq!(spawns[0].0, "vfx/Impact");
    assert_eq!(spawns[0].1, [1.0, 2.0, 3.0]);
    assert!(host.take_spawn_effects().is_empty(), "drained");
}

#[test]
fn getcomponent_toggles_particle_play_on_start() {
    let dir = std::env::temp_dir().join("floptle_script_test_vfx_comp");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "arm",
        "function update(node, dt)\n  node:getcomponent('ParticleSystem').play_on_start = 1\nend\n",
    );
    let (mut world, e) = world_with_script("arm");
    world.insert(e, ParticleSystem { asset: "vfx/Smoke".into(), play_on_start: false });
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);
    assert!(world.get::<ParticleSystem>(e).unwrap().play_on_start, "field must flush to the ECS");
}

/// The sprite component as a HANDLE — `node:sprite()` — plus the call that
/// used to fail in total silence.
///
/// A 2D character flips on a turn, which is one boolean written every frame.
/// The only route was `node:setSprite{ flipX = }`, and written positionally
/// — `setSprite{ 8, 1, flipX }`, which is what somebody reaching for a
/// six-argument call writes — every key read back as absent, so the call
/// re-set the sprite to exactly what it already was: the print said `true`,
/// the Inspector said nothing, and there was no error anywhere.
#[test]
fn a_script_reads_and_writes_the_sprite_component() {
    use floptle_core::Matter;

    let dir = std::env::temp_dir().join("floptle_script_test_sprite_handle");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "hero",
        concat!(
            "function start(node)\n",
            "  local sp = node:sprite()\n",
            "  sp.flipX = true\n",
            "  sp.cell = 4\n",
            "  sp.pivotY = 0\n",
            // Read-your-writes: the queue applies after the pass, so a
            // handle that could only see the mirror would answer with the
            // value from before the line above it.
            "  log('flipX reads ' .. tostring(sp.flipX))\n",
            "  log('cell reads ' .. tostring(sp.cell))\n",
            // The generic component route has to answer with a BOOLEAN:
            // 0 is truthy in Lua, so a number makes `if sp.flipY then`
            // always taken.
            "  local c = node:getcomponent('Sprite')\n",
            "  log('component flipY is a ' .. type(c.flipY))\n",
            // And the positional call raises instead of doing nothing.
            "  local ok, err = pcall(function() node:setSprite{ 8, 1, true } end)\n",
            "  log('positional: ' .. tostring(ok) .. ' ' .. tostring(err))\n",
            "end\n",
        ),
    );

    let mut world = World::default();
    let hero = world.spawn();
    world.insert(hero, Transform::IDENTITY);
    world.insert(
        hero,
        Matter::Sprite {
            ppu: 32.0,
            size: 1.0,
            cell: 0,
            flip_x: false,
            flip_y: false,
            pivot: [0.5, 0.5],
        },
    );
    world.insert(
        hero,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "hero".into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());

    let Some(Matter::Sprite { cell, flip_x, pivot, ppu, .. }) = world.get::<Matter>(hero)
    else {
        panic!("the node stopped being a sprite")
    };
    assert!(*flip_x, "sp.flipX = true must reach the component the renderer reads");
    assert_eq!(*cell, 4, "sp.cell = 4 must reach the component");
    assert_eq!(pivot[1], 0.0, "sp.pivotY = 0 puts the origin at the feet");
    assert_eq!(pivot[0], 0.5, "…and leaves the axis it did not name alone");
    assert_eq!(*ppu, 32.0, "an untouched field keeps what the node had");

    let logs: Vec<String> = host.drain_logs().into_iter().map(|l| l.msg).collect();
    let said = |what: &str| {
        assert!(logs.iter().any(|l| l.contains(what)), "no log said {what:?}: {logs:?}");
    };
    said("flipX reads true");
    said("cell reads 4");
    said("component flipY is a boolean");
    said("positional: false");
    said("setSprite");
    assert!(
        logs.iter().any(|l| l.contains("positional") && l.contains("pivotX")),
        "the refusal must name the keys it does read: {logs:?}"
    );
}

/// A sprite frame lands on whichever thing owns the cell.
///
/// A `Matter::Sprite` carries its own `cell` and its Material's is unused,
/// so writing the Material's for a Sprite node would set a number the
/// Inspector shows and nothing draws — the worst kind of wrong, because it
/// looks like it worked.
#[test]
fn a_sprite_frame_writes_the_cell_the_node_actually_reads() {
    use floptle_core::{Material, Matter};

    // A plane wearing a material: the Material's cell is the live one.
    let mut world = World::default();
    let plane = world.spawn();
    world.insert(plane, Transform::IDENTITY);
    world.insert(plane, Matter::Primitive { shape: floptle_core::Shape::Plane, color: [1.0; 3] });
    world.insert(plane, Material::default());
    crate::apply_sprite_frame(&mut world, plane, "art/hero.png", 8, 4, 5);
    let m = world.get::<Material>(plane).unwrap();
    assert_eq!(m.texture.as_deref(), Some("art/hero.png"));
    assert_eq!((m.sheet_cols, m.sheet_rows, m.cell), (8, 4, 5));

    // A Sprite node: the cell is on the Matter, and the Material's must be
    // left alone rather than set to a number nothing looks at.
    let sprite = world.spawn();
    world.insert(sprite, Transform::IDENTITY);
    world.insert(
        sprite,
        Matter::Sprite { ppu: 32.0, size: 1.0, cell: 0, flip_x: false, flip_y: false, pivot: [0.5, 0.5] },
    );
    world.insert(sprite, Material::default());
    crate::apply_sprite_frame(&mut world, sprite, "art/hero.png", 8, 4, 5);
    let Some(Matter::Sprite { cell, .. }) = world.get::<Matter>(sprite) else {
        panic!("not a sprite any more")
    };
    assert_eq!(*cell, 5, "the Sprite's own cell is the one that draws");
    let m = world.get::<Material>(sprite).unwrap();
    assert_eq!((m.sheet_cols, m.sheet_rows), (8, 4), "the grid is still the Material's");
    assert_eq!(m.cell, 0, "the Material's cell is unused here and must not be written");

    // …and reading it back gives what playing it put in — the two halves a
    // record-then-play round trip depends on.
    assert_eq!(
        crate::read_sprite_frame(&world, sprite),
        Some(("art/hero.png".to_string(), 8, 4, 5))
    );
    assert_eq!(
        crate::read_sprite_frame(&world, plane),
        Some(("art/hero.png".to_string(), 8, 4, 5))
    );
}

/// `anim:events` / `anim:duration` expose the AUTHORED clip data so a game can bake
/// integer frame data at load, instead of letting float playback events drive
/// gameplay (which stepped playback quantises and a prediction replay never re-fires).
/// They read the asset mirror, so they answer in `start()` — before anything has
/// played a frame.
#[test]
fn animator_exposes_authored_clip_events_and_duration() {
    let dir = std::env::temp_dir().join("floptle_script_test_anim_events");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "bake",
        "function start(node)\n\
           local a = node:animator()\n\
           local dur = a:duration('Punch')\n\
           local evs = a:events('Punch')\n\
           -- 12 gameplay frames over the clip: which frame is the hitbox on?\n\
           for _, e in ipairs(evs) do\n\
             if e.func == 'onHitboxStart' then\n\
               node.x = math.floor(e.t / dur * 12 + 0.5)\n\
             end\n\
           end\n\
           node.y = #evs\n\
           node.z = (a:events('NoSuchClip') == nil) and 1 or 0\n\
         end\n\
         function update(node, dt) end\n",
    );
    let (mut world, e) = world_with_script("bake");
    let mut host = ScriptHost::new();
    host.set_anim_info(HashMap::from([(
        e.index(),
        AnimInfo {
            layers: vec![("Base".into(), None, 0.0, false)],
            clips: Rc::new(vec![ClipInfo {
                name: "Punch".into(),
                duration: 0.5,
                events: vec![(0.125, "onHitboxStart".into()), (0.25, "onHitboxEnd".into())],
            }]),
        },
    )]));
    host.run(&mut world, &dir, 0.1, 0.1);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let tr = world.get::<Transform>(e).unwrap().translation;
    assert_eq!(tr.x, 3.0, "0.125s of a 0.5s clip over 12 frames is frame 3");
    assert_eq!(tr.y, 2.0, "both authored events came through");
    assert_eq!(tr.z, 1.0, "an unknown clip reads nil rather than erroring");
}
