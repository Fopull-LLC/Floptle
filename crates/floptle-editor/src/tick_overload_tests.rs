//! When a fixed tick costs more than the time it simulates, the game slows
//! down instead of falling to a few frames a second, and says so once.
//!
//! Driven the way a window drives the editor: each frame is handed the wall
//! time the previous one took. That feedback is what turns an expensive tick
//! into a spiral — a long frame banks more ticks, which make the next frame
//! longer — so a test that stepped a fixed `dt` could not see it at all.

use floptle_core::time::Instant;

use floptle_core::{Matter, Name, ScriptInst, Scripts, Transform};

/// A project whose one script burns `ms` of wall time in every `fixedUpdate`.
fn heavy_project(root: &std::path::Path, ms: f64) {
    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::write(
        root.join("scripts/heavy.lua"),
        format!(
            "function fixedUpdate(node, dt)\n\
               local t = os.clock()\n\
               while os.clock() - t < {} do end\n\
             end\n",
            ms / 1000.0
        ),
    )
    .unwrap();
}

/// Frames rendered in `seconds` of wall time, and what the Console said.
fn play_for(dir: &std::path::Path, policy: floptle_scene::TickOverloadDoc, seconds: f64) -> (usize, Vec<String>) {
    let mut ed = crate::Editor { project_root: dir.to_path_buf(), ..Default::default() };
    ed.project.tick_overload = policy;
    let e = ed.world.spawn();
    ed.world.insert(e, Transform::IDENTITY);
    ed.world.insert(e, Name("Heavy".into()));
    ed.world.insert(e, Matter::Empty);
    ed.world.insert(
        e,
        Scripts(vec![ScriptInst { kind: "heavy".into(), enabled: true, params: Vec::new(), refs: Vec::new(), strs: Vec::new(), pinned_bools: None }]),
    );
    ed.toggle_play();
    assert!(ed.playing, "did not enter play");
    let start = Instant::now();
    let mut last = 1.0f32 / 60.0;
    let mut frames = 0;
    // The first second is detection; count the frames after it.
    let mut counted = 0;
    while start.elapsed().as_secs_f64() < seconds {
        let t0 = Instant::now();
        ed.play_step(last, true);
        ed.drain_script_logs();
        ed.script_host.profile().borrow_mut().end_frame();
        last = t0.elapsed().as_secs_f32().min(0.25);
        frames += 1;
        if start.elapsed().as_secs_f64() > 1.0 {
            counted += 1;
        }
    }
    assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    assert!(frames > 0);
    let said = ed.console.entries.iter().map(|e| e.msg.clone()).collect();
    (counted, said)
}

/// **An overloaded tick plays in slow motion, not at 2 fps.** Thirty
/// milliseconds a tick against a 16.7 ms slice: catching up runs the maximum
/// ticks every frame and settles near 4 fps; slow motion runs one a frame and
/// stays above 15. The Console says it once, with the numbers.
#[test]
fn an_overloaded_tick_plays_in_slow_motion_and_says_so_once() {
    let dir = std::env::temp_dir().join(format!("floptle_overload_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    heavy_project(&dir, 30.0);
    let secs = 3.5;
    let (slow, said) = play_for(&dir, floptle_scene::TickOverloadDoc::SlowMotion, secs);
    let (catch, _) = play_for(&dir, floptle_scene::TickOverloadDoc::CatchUp, secs);
    let fps = |n: usize| n as f64 / (secs - 1.0);
    assert!(fps(slow) >= 15.0, "slow motion ran at {:.1} fps", fps(slow));
    assert!(fps(catch) < fps(slow) / 2.0, "catch-up {:.1} fps vs slow motion {:.1}: no spiral to escape?", fps(catch), fps(slow));
    let warnings: Vec<&String> = said.iter().filter(|m| m.contains("more than its")).collect();
    assert_eq!(warnings.len(), 1, "said {} times: {said:#?}", warnings.len());
    let w = warnings[0];
    assert!(w.contains("16.7 ms slice") && w.contains("slow motion") && w.contains("scripts"), "{w}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **The tick clock keeps up with the frame clock after a long load.** A
/// 30-second frame banks far more than the catch-up cap allows, and the
/// ticks it drops used to be time the tick clock never got back: `time` in
/// `fixedUpdate` trailed `time` in `update` by the dropped seconds for the
/// rest of the session, so a deadline set in one pass and checked in the
/// other was never reached. After the load, for 10 s of frames, the two stay
/// within one tick.
#[test]
fn the_tick_clock_keeps_up_after_a_long_load() {
    let dir = std::env::temp_dir().join(format!("floptle_tick_clock_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(
        dir.join("scripts/clocks.lua"),
        "function update(node, dt) ut = time end\nfunction fixedUpdate(node, dt) ft = time end\n",
    )
    .unwrap();
    let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
    let e = ed.world.spawn();
    ed.world.insert(e, Transform::IDENTITY);
    ed.world.insert(e, Name("Clocks".into()));
    ed.world.insert(e, Matter::Empty);
    ed.world.insert(e, Scripts(vec![ScriptInst::new("clocks")]));
    ed.toggle_play();
    assert!(ed.playing, "did not enter play");
    let step = ed.game_tick.step as f64;
    ed.play_step(1.0 / 60.0, true);
    // The load: one frame that took 30 s.
    ed.play_step(30.0, true);
    let mut worst = 0.0f64;
    for _ in 0..600 {
        ed.play_step(1.0 / 60.0, true);
        let env = ed.script_host.instance_env(e.index(), "clocks").expect("ran");
        let (ut, ft): (f64, f64) = (env.get("ut").unwrap(), env.get("ft").unwrap());
        worst = worst.max((ut - ft).abs());
    }
    assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    assert!(worst < step + 1e-3, "update's time and fixedUpdate's drifted {worst:.3} s apart (a tick is {step:.4} s)");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **Each frame's ticks read that frame's clock.** At 20 fps a frame runs
/// three ticks; they read the clock one frame behind `update` (6.004–6.038
/// against `update`'s 6.100), and across a 0.9 s hitch a whole hitch behind,
/// so a deadline `update` set was seen up to a hitch late. Every tick of a
/// frame now falls after the previous frame's `update` time and at or before
/// this one's, the last within a tick of it, through a hitch as well.
#[test]
fn every_tick_reads_its_own_frames_clock() {
    let dir = std::env::temp_dir().join(format!("floptle_tick_frame_clock_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(
        dir.join("scripts/clocks.lua"),
        "ticks = {}\nfunction update(node, dt) ut = time; ticks = {} end\n\
         function fixedUpdate(node, dt) ticks[#ticks + 1] = time end\n",
    )
    .unwrap();
    let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
    let e = ed.world.spawn();
    ed.world.insert(e, Transform::IDENTITY);
    ed.world.insert(e, Name("Clocks".into()));
    ed.world.insert(e, Matter::Empty);
    ed.world.insert(e, Scripts(vec![ScriptInst::new("clocks")]));
    ed.toggle_play();
    assert!(ed.playing, "did not enter play");
    let step = ed.game_tick.step as f64;
    ed.play_step(1.0 / 20.0, true);
    let mut prev: f64 = ed.script_host.instance_env(e.index(), "clocks").expect("ran").get("ut").unwrap();
    // 20 fps, a 0.9 s hitch, 20 fps again.
    let frames: Vec<f32> = (0..20).map(|_| 0.05).chain([0.9]).chain((0..20).map(|_| 0.05)).collect();
    let mut checked = 0;
    for (n, dt) in frames.into_iter().enumerate() {
        ed.play_step(dt, true);
        let env = ed.script_host.instance_env(e.index(), "clocks").expect("ran");
        let ut: f64 = env.get("ut").unwrap();
        let ticks: Vec<f64> = env.get::<mlua::Table>("ticks").unwrap().sequence_values::<f64>().map(Result::unwrap).collect();
        for t in &ticks {
            assert!(*t > prev - 1e-4 && *t <= ut + 1e-4, "frame {n}: a tick read {t:.4}, outside this frame ({prev:.4}, {ut:.4}]: {ticks:?}");
            checked += 1;
        }
        if let Some(last) = ticks.last() {
            assert!(ut - last < step + 1e-4, "frame {n}: the last tick read {last:.4}, {:.4} s behind update's {ut:.4}", ut - last);
        }
        prev = ut;
    }
    assert!(checked > 40, "too few ticks ran to say anything: {checked}");
    assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Ticks each frame ran over `seconds` of wall time, after the first half
/// second, for a project whose `fixedUpdate` burns `tick_ms` and whose
/// `update` burns `frame_ms`, under the given catch-up budget.
fn ticks_per_frame(dir: &std::path::Path, tick_ms: f64, frame_ms: f64, budget: Option<f32>, seconds: f64) -> Vec<u64> {
    std::fs::create_dir_all(dir.join("scripts")).unwrap();
    std::fs::write(
        dir.join("scripts/dear.lua"),
        format!(
            "local function burn(s) local t = os.clock() while os.clock() - t < s do end end\n\
             function update(node, dt) burn({}) end\n\
             function fixedUpdate(node, dt) burn({}) end\n",
            frame_ms / 1000.0,
            tick_ms / 1000.0
        ),
    )
    .unwrap();
    let mut ed = crate::Editor { project_root: dir.to_path_buf(), ..Default::default() };
    // Catch up, so the slow-motion watch cannot step in for the control: a
    // budget has to bound the ticks on its own.
    ed.project.tick_overload = floptle_scene::TickOverloadDoc::CatchUp;
    ed.project.tick_budget_ms = budget;
    let e = ed.world.spawn();
    ed.world.insert(e, Transform::IDENTITY);
    ed.world.insert(e, Name("Dear".into()));
    ed.world.insert(e, Matter::Empty);
    ed.world.insert(e, Scripts(vec![ScriptInst::new("dear")]));
    ed.toggle_play();
    assert!(ed.playing, "did not enter play");
    let start = Instant::now();
    let mut last = 1.0f32 / 60.0;
    let mut out = Vec::new();
    while start.elapsed().as_secs_f64() < seconds {
        let t0 = Instant::now();
        let ticks0 = ed.game_tick_no;
        ed.play_step(last, true);
        ed.drain_script_logs();
        last = t0.elapsed().as_secs_f32().min(0.25);
        if start.elapsed().as_secs_f64() > 0.5 {
            out.push(ed.game_tick_no - ticks0);
        }
    }
    assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    out
}

/// **A catch-up budget keeps an expensive tick from compounding.** A 14 ms
/// tick is under its 16.7 ms slice, so the overload watch never fires, and
/// with 10 ms of other work a frame it still spirals: each long frame banks
/// more ticks for the next, and frames settle at several ticks each. Under a
/// 20 ms budget no frame runs more than one, and with no budget set (every
/// project before it existed) the frames compound as they always did.
///
/// Counts, not times: on a slower machine the ticks only get dearer, which
/// makes the control compound harder and leaves the budgeted run at one.
#[test]
fn a_catch_up_budget_bounds_the_ticks_a_frame_runs() {
    let dir = std::env::temp_dir().join(format!("floptle_tick_budget_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let bounded = ticks_per_frame(&dir, 14.0, 10.0, Some(20.0), 2.0);
    let free = ticks_per_frame(&dir, 14.0, 10.0, None, 2.0);
    assert!(bounded.len() > 10 && free.len() > 5, "too few frames: {} / {}", bounded.len(), free.len());
    let most = bounded.iter().copied().max().unwrap_or(0);
    assert_eq!(most, 1, "a 20 ms budget ran {most} ticks of 14 ms in one frame: {bounded:?}");
    let mean = free.iter().sum::<u64>() as f64 / free.len() as f64;
    assert!(mean >= 2.0, "with no budget the ticks should compound, ran {mean:.2} a frame: {free:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **A physics frame names its stages and the body that cost it.** A ball
/// dropped onto a blockout floor, played with the profiler on: every frame
/// that ran a tick records physics parts, the static stage among them, and
/// names the ball by its node name in the frame's costliest bodies and in
/// `perf.bodies`' source. Off, the sim times nothing.
#[test]
fn a_physics_frame_names_its_stages_and_its_body() {
    use floptle_core::math::{DVec3, Vec3};
    let mut ed = crate::Editor::default();
    let g = ed.world.spawn();
    ed.world.insert(g, Transform::IDENTITY);
    ed.world.insert(g, Matter::GravityVolume { mode: floptle_core::GravityMode::Down, strength: 9.81, radius: 500.0 });
    ed.maps.meshes.insert(100, floptle_map::box_mesh(Vec3::new(6.0, 0.1, 6.0)));
    let floor = ed.world.spawn();
    ed.world.insert(floor, Transform::from_translation(DVec3::new(0.0, -0.1, 0.0)));
    ed.world.insert(floor, Matter::MapMesh { id: 100 });
    ed.world.insert(floor, floptle_core::Collidable);
    let ball = ed.world.spawn();
    ed.world.insert(ball, Transform::from_translation(DVec3::new(0.0, 3.0, 0.0)));
    ed.world.insert(ball, Name("Ball".into()));
    ed.world.insert(ball, Matter::Primitive { shape: floptle_core::Shape::Sphere, color: [1.0; 3] });
    let rb = ed.default_body_for(ball);
    ed.world.insert(ball, rb);
    ed.toggle_play();
    assert!(ed.playing, "did not enter play");
    // Off: no timing reaches the sim.
    ed.play_step(1.0 / 60.0, true);
    assert_eq!(ed.sim.as_mut().and_then(|s| s.world.take_timing()), None, "the sim timed steps nobody asked for");
    ed.script_host.profile().borrow_mut().enable(true);
    let mut named = 0;
    for _ in 0..30 {
        let ticks0 = ed.game_tick_no;
        ed.play_step(1.0 / 60.0, true);
        ed.script_host.profile().borrow_mut().end_frame();
        if ed.game_tick_no == ticks0 {
            continue;
        }
        let p = ed.script_host.profile().borrow();
        let r = p.recent_frames(1).next().expect("a frame");
        assert!(r.part("physicsStatic") > 0.0, "a tick ran with no static stage: {:?}", r.parts);
        assert!(r.parts.iter().any(|(n, _)| *n == "physicsBroadphase"), "{:?}", r.parts);
        if r.bodies.iter().any(|(n, ms)| n == "Ball" && *ms > 0.0) {
            named += 1;
        }
        assert_eq!(p.last_bodies(), r.bodies.as_slice());
    }
    assert!(named >= 20, "the ball was named on {named} of the frames that ticked");
}
