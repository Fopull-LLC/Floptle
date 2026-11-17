use super::*;

/// The probe: a known slow function and a known run of engine calls, both
/// in one script's `update`, with the profiler and sampler switched on from
/// the script itself on its first frame.
const HOT: &str = "local function grind(n)\n\
  local x = 0\n\
  for i = 1, n do x = x + i % 7 end return x\n\
end\n\
local function casts()\n\
  for i = 1, 300 do spherecast(vec3(0, 5, 0), vec3(0, -1, 0), 0.5, 10) end\n\
end\n\
local frame = 0\n\
function update(node, dt)\n\
  frame = frame + 1\n\
  if frame == 1 then perf.enable(true) perf.sampleScripts(true, 0.05) perf.hitchThreshold(5) return end\n\
  grind(4000000)\n\
  casts()\n\
end\n";

/// **A slow frame names the function, the line and the engine calls it was
/// spent on.** One script spends four million iterations (tens of ms) on
/// line 3 of `grind` and
/// casts 300 spheres from line 6. The frame's record names `grind`:3 as the
/// hottest line and charges `grind` within 10% of the frame's measured CPU
/// time less its casts; the casts are charged to the script under `spherecast`, 300 of
/// them, and their line is charged; and the frame is a hitch at the 5 ms
/// threshold the script set, carrying the same samples.
///
/// Every time here is compared with another measured in the same frame, so a
/// loaded machine stretches both sides alike.
#[test]
fn a_slow_frame_names_its_function_line_and_engine_calls() {
    let dir = std::env::temp_dir().join(format!("floptle-perf-sampler-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_script(&dir, "hot", HOT);
    let (mut world, _) = world_with_script("hot");
    let mut host = ScriptHost::new();
    host.set_colliders(
        vec![floptle_physics::AnchoredCollider::world(Box::new(floptle_physics::Plane::ground(0.0)))],
        glam::DVec3::ZERO,
    );
    let mut run_cpu = 0.0f32;
    for i in 1..=2 {
        let c0 = crate::budget::thread_cpu();
        host.run(&mut world, &dir, 0.016, 0.016 * i as f32);
        if let (Some(a), Some(b)) = (c0, crate::budget::thread_cpu()) {
            run_cpu = b.saturating_sub(a).as_secs_f32() * 1000.0;
        }
        host.profile().borrow_mut().end_frame();
        assert!(host.errors().is_empty(), "frame {i}: {:?}", host.errors());
    }
    let mut prof = host.profile().borrow_mut();
    let r = prof.recent_frames(1).next().expect("a frame").clone();
    let script_ms = r.scripts.iter().find(|(n, _)| n == "hot").map(|(_, ms)| *ms).expect("hot ran");
    let cast = r.calls.iter().find(|c| c.script == "hot" && c.call == "spherecast").expect("casts charged to hot");
    assert_eq!(cast.n, 300, "{:?}", r.calls);
    let top = &r.samples[0];
    assert_eq!((top.script.as_str(), top.func.as_str(), top.line), ("hot", "grind", 3), "{:?}", r.samples);
    // The sampler charges this thread's CPU time, so a loaded machine taking
    // the core away mid-spin lands on nobody; the frame's own CPU time is what
    // it is held to. Where the platform has no thread clock it charges the
    // wall, and so is held to the script's wall time.
    let grind: f32 = r.samples.iter().filter(|s| s.func == "grind").map(|s| s.ms).sum();
    let measured = if run_cpu > 0.0 { run_cpu } else { script_ms };
    let expect = measured - cast.ms;
    assert!(
        (grind - expect).abs() <= expect * 0.1,
        "grind sampled {grind:.2} ms against {expect:.2} ms measured ({measured:.2} less {:.2} of casts)",
        cast.ms
    );
    // The line that makes the calls is named and charged. Whether a long call's
    // whole time lands on it is `sampler.rs`'s own guard, which times the call
    // in the clock the sampler charges. Here the casts are timed on the wall and
    // the samples in CPU time, so a moment the OS took the core away mid-sweep
    // (6.5 ms of wall against 1.4 ms charged, once, in a loaded suite) would
    // compare two different clocks.
    let casts_line: f32 = r.samples.iter().filter(|s| s.func == "casts" && s.line == 6).map(|s| s.ms).sum();
    assert!(casts_line > 0.0, "the casts' line 6 was never charged: {:?}", r.samples);
    let h = prof.take_hitches();
    assert_eq!(h.len(), 1, "the script's 5 ms threshold makes the slow frame a hitch: {h:?}");
    assert_eq!(h[0].samples.first().map(|s| (s.func.as_str(), s.line)), Some(("grind", 3)));
    assert_eq!(h[0].calls.first().map(|c| c.call), Some("spherecast"));
    drop(prof);
    let _ = std::fs::remove_dir_all(&dir);
}

/// **The script bucket's time outside every hook is named.** A timer callback
/// that spins for 20 ms runs in no hook, so no script's figure carries it, and
/// it used to sit in the bucket as time nobody could see. It is charged to
/// `scriptsTimers` now, and the rest of the pass is in its own named parts:
/// the work list, the write flush, and the engine between the hooks.
#[test]
fn script_time_outside_the_hooks_is_named() {
    let dir = std::env::temp_dir().join(format!("floptle-perf-parts-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "timed",
        "function start(node)\n  \
           after(0.001, function() local t = os.clock() while os.clock() - t < 0.02 do end end)\n\
         end\n\
         function fixedUpdate(node, dt) end\n",
    );
    let (mut world, _) = world_with_script("timed");
    let mut host = ScriptHost::new();
    host.profile().borrow_mut().enable(true);
    host.profile().borrow_mut().begin_frame();
    host.run(&mut world, &dir, 0.016, 0.016);
    host.profile().borrow_mut().end_frame();
    host.profile().borrow_mut().begin_frame();
    host.run_fixed(&mut world, 0.1, 0.1);
    host.profile().borrow_mut().end_frame();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    let prof = host.profile().borrow();
    let r = prof.recent_frames(1).next().expect("a frame").clone();
    let part = |name: &str| r.parts.iter().find(|(p, _)| *p == name).map(|(_, ms)| *ms);
    let timers = part("scriptsTimers").unwrap_or(0.0);
    assert!(timers >= 15.0, "the 20 ms timer callback was charged {timers:.2} ms to scriptsTimers: {:?}", r.parts);
    for name in ["scriptsWorkList", "scriptsFlush", "scriptsBetweenHooks"] {
        assert!(part(name).is_some(), "{name} is not named: {:?}", r.parts);
    }
    let hooks: f32 = r.scripts.iter().map(|(_, ms)| *ms).sum();
    // Against the timer's own time rather than a fixed few milliseconds: a
    // loaded machine stretches both, and one at load 16 once stretched the
    // hooks past 5 ms. A spin charged to a hook reads the whole 20 ms there.
    assert!(hooks < 0.5 * timers, "the timer's spin was charged to a hook ({hooks:.2} ms against the timer's {timers:.2})");
}

/// **A file written on the frame is named as such.** `assets.writeText`
/// without `async` holds the frame while the disk writes. On a busy disk
/// that was seconds, and it read as script time. Its time is charged to the
/// `fileWriteOnFrame` part now.
#[test]
fn a_file_written_on_the_frame_is_named() {
    let dir = std::env::temp_dir().join(format!("floptle-perf-filewrite-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "writer",
        "function update(node, dt)\n  \
           local ok, why = assets.writeText(\"user://perf_note.txt\", string.rep(\"x\", 4096))\n  \
           if not ok then error(why) end\n\
         end\n",
    );
    let (mut world, _) = world_with_script("writer");
    let mut host = ScriptHost::new();
    host.set_project_root(dir.clone());
    host.profile().borrow_mut().enable(true);
    host.profile().borrow_mut().begin_frame();
    host.run(&mut world, &dir, 0.016, 0.016);
    host.profile().borrow_mut().end_frame();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    let prof = host.profile().borrow();
    let r = prof.recent_frames(1).next().expect("a frame").clone();
    assert!(r.parts.iter().any(|(p, _)| *p == "fileWriteOnFrame"), "the on-frame write is not named: {:?}", r.parts);
}

/// **The collector's work between frames is named, and it collects.** A
/// script leaves a few MB of garbage; stepping the collector between frames
/// (`step_gc`, what the engine does each frame) records `scriptsGc`, and
/// stepped through two whole cycles, the heap is smaller than it was.
#[test]
fn the_collector_steps_between_frames_and_is_named() {
    let dir = std::env::temp_dir().join(format!("floptle-perf-gc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "litter",
        "function update(node, dt)\n  \
           local junk = {}\n  \
           for i = 1, 60000 do junk[i] = { i, tostring(i) } end\n\
         end\n",
    );
    let (mut world, _) = world_with_script("litter");
    let mut host = ScriptHost::new();
    host.profile().borrow_mut().enable(true);
    host.profile().borrow_mut().begin_frame();
    host.run(&mut world, &dir, 0.016, 0.016);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    let before = host.lua_used_memory();
    host.step_gc(0.5);
    host.profile().borrow_mut().end_frame();
    let parts = host.profile().borrow().recent_frames(1).next().expect("a frame").parts.clone();
    assert!(parts.iter().any(|(n, ms)| *n == "scriptsGc" && *ms > 0.0), "the collector's step is not named: {parts:?}");
    // Two whole cycles: the first to finish may be one that began before
    // the junk was garbage, and frees none of it.
    let (mut cycles, mut steps) = (0, 0);
    while cycles < 2 && steps < 5000 {
        if host.step_gc(5.0) {
            cycles += 1;
        }
        steps += 1;
    }
    let after = host.lua_used_memory();
    assert!(after < before, "stepping the collector to the end of a cycle freed nothing: {before} -> {after} bytes");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **The collector's work while scripts run is counted per frame, and the
/// engine's own paced steps are not.** A script that leaves megabytes of
/// garbage every frame makes Luau take collector steps inside its hook; the
/// frame's `gcMs` is above zero, within the hook's own time, and readable from
/// Lua with `mainCpuMs` beside it. A frame where no script runs but the
/// engine steps the collector between frames (`step_gc`, the `scriptsGc`
/// part) reads zero.
#[test]
fn collector_time_inside_hooks_is_counted_and_paced_steps_are_not() {
    let dir = std::env::temp_dir().join(format!("floptle-perf-gcms-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "litter",
        "local frame = 0\n\
         function update(node, dt)\n  \
           frame = frame + 1\n  \
           if frame == 1 then perf.enable(true) return end\n  \
           local junk = {}\n  \
           for i = 1, 60000 do junk[i] = { i, tostring(i) } end\n  \
           if frame == 3 then\n    \
             local f = perf.frames(1)[1]\n    \
             assert(type(f.gcMs) == 'number', 'gcMs is not a number')\n    \
             assert(f.gcMs > 0, 'a littering frame read no collector time')\n    \
             assert(f.mainCpuMs == nil or f.mainCpuMs >= 0, 'mainCpuMs is not a time')\n  \
           end\n\
         end\n",
    );
    let (mut world, _) = world_with_script("litter");
    let mut host = ScriptHost::new();
    for i in 1..=3 {
        host.profile().borrow_mut().begin_frame();
        host.run(&mut world, &dir, 0.016, 0.016 * i as f32);
        assert!(host.errors().is_empty(), "frame {i}: {:?}", host.errors());
        host.profile().borrow_mut().end_frame();
    }
    let littered = host.profile().borrow().recent_frames(1).next().expect("a frame").clone();
    assert!(littered.gc_ms > 0.0, "a frame that littered megabytes read no collector time");
    let hook = littered.scripts.iter().find(|(n, _)| n == "litter").map(|(_, ms)| *ms).expect("litter ran");
    assert!(littered.gc_ms <= hook, "collector time {} is more than the hook it fell in, {hook}", littered.gc_ms);
    assert_eq!(littered.main_cpu_ms.is_some(), crate::budget::thread_cpu().is_some());
    host.profile().borrow_mut().begin_frame();
    host.step_gc(2.0);
    host.profile().borrow_mut().end_frame();
    let paced = host.profile().borrow().recent_frames(1).next().expect("a frame").clone();
    assert!(paced.part("scriptsGc") > 0.0, "the paced step did not run: {:?}", paced.parts);
    assert_eq!(paced.gc_ms, 0.0, "the engine's paced collector steps were counted as the scripts'");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A world of `n` nodes running `kind`, then `busy` more running `"busy"`.
fn scripted_crowd(kind: &str, n: usize, busy: usize) -> World {
    let mut world = World::default();
    for i in 0..n + busy {
        let e = world.spawn();
        world.insert(e, Transform::from_translation(glam::DVec3::new(i as f64, 0.0, 0.0)));
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: if i < n { kind.into() } else { "busy".into() },
                enabled: true,
                params: vec![("speed".into(), 2.0)],
                refs: Vec::new(),
                strs: Vec::new(),
                pinned_bools: None,
            }]),
        );
    }
    world
}

/// One frame the way the editor runs it — the frame pass, a tick, the late
/// pass — each timed into the scripts bucket net of the mirror, as
/// `timed_script_pass` does, so `scriptsUnnamed` is what it is in a game.
fn profiled_frame(host: &mut ScriptHost, world: &mut World, dir: &Path, f: usize) -> floptle_core::profile::FrameRecord {
    use floptle_core::profile::{Bucket, Span};
    let t = f as f32 / 60.0;
    host.profile().borrow_mut().begin_frame();
    for pass in 0..3 {
        let mirror0 = host.profile().borrow().frame_total(Bucket::Mirror);
        let span = Span::new();
        match pass {
            0 => host.run(world, dir, 1.0 / 60.0, t),
            1 => host.run_fixed(world, 1.0 / 60.0, t),
            _ => host.run_late(world, 1.0 / 60.0, t),
        }
        let ms = span.ms();
        let mut p = host.profile().borrow_mut();
        let mirror = p.frame_total(Bucket::Mirror) - mirror0;
        p.record(Bucket::Scripts, (ms - mirror).max(0.0));
    }
    host.profile().borrow_mut().end_frame();
    host.profile().borrow().recent_frames(1).next().expect("a frame").clone()
}

/// **An instance with no hook for a pass costs next to nothing in it.**
/// The script pass's time outside every hook — `scriptsSeed`,
/// `scriptsHookless`, `scriptsBetweenHooks` and whatever is left in
/// `scriptsUnnamed` — for 2,000 instances of a script that does nothing per
/// frame, beside 50 running an `update`, over the frame pass, a tick and the
/// late pass. Two such scripts: a library with no hooks at all, and one whose
/// only hook is `start`.
///
/// Held to the `update` instances' own hook time measured in the same frames,
/// per instance, so a loaded machine stretches both sides alike (best of
/// three runs, since a burst of load can land on either side). Both cost more
/// than twice what a busy instance did: every enabled instance was seeded
/// every frame (two `String` keys, two registry fetches, a fresh `node`
/// handle and registry slot), walked the whole hook road three passes a frame
/// for hooks it does not have, and had its `node` table drained and
/// re-stamped each time. Watched failing at 2.2 times (11.4 µs an idle
/// instance against 5.2 µs a busy one, `scriptsSeed` 3.1 ms and
/// `scriptsUnnamed` 18.6 ms a frame).
///
/// The library must cost under 30% of a busy instance (0.13 after). The
/// `start`-only script is held to 0.4: `start` handed it its `node` table,
/// which a callback may still write through, so the table is checked every
/// pass for as long as the instance lives. Read field by field that was
/// about 0.8 µs a pass at rest (1.06; 1.45 with the at-rest check off).
/// Asked of the table's armed flag once a refresh had begun, 0.51; asked
/// before the refresh, as now, 0.29.
#[test]
fn an_instance_without_a_hook_costs_next_to_nothing_outside_the_hooks() {
    let dir = std::env::temp_dir().join(format!("floptle-perf-hookless-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_script(&dir, "busy", "function update(node, dt) node.y = node.y + dt * params.speed end\n");
    write_script(
        &dir,
        "lib",
        "defaults = { speed = 1 }\nlocal count = 0\nfunction bump() count = count + 1 return count end\n",
    );
    write_script(&dir, "starter", "defaults = { speed = 1 }\nfunction start(node) node.x = node.x + 0 end\n");
    let (idle, busy, frames) = (2000usize, 50usize, 60usize);
    // Timed as a game runs, without re-reading every armed table.
    crate::env::NO_CROSS_CHECK.with(|c| c.set(true));
    for (kind, bound) in [("lib", 0.3f32), ("starter", 0.4)] {
        let mut tries = String::new();
        let mut passed = false;
        for _ in 0..3 {
            let mut world = scripted_crowd(kind, idle, busy);
            let mut host = ScriptHost::new();
            host.profile().borrow_mut().enable(true);
            for f in 0..5 {
                profiled_frame(&mut host, &mut world, &dir, f);
            }
            assert!(host.errors().is_empty(), "{:?}", host.errors());
            let (mut seed, mut unnamed, mut hookless, mut between, mut hooks) = (0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for f in 5..5 + frames {
                let r = profiled_frame(&mut host, &mut world, &dir, f);
                seed += r.part("scriptsSeed");
                unnamed += r.part("scriptsUnnamed");
                hookless += r.part("scriptsHookless");
                between += r.part("scriptsBetweenHooks");
                hooks += r.scripts.iter().filter(|(n, _)| n == "busy").map(|(_, ms)| *ms).sum::<f32>();
            }
            assert!(hooks > 0.0, "the busy instances never ran");
            let per_idle = (seed + unnamed + hookless + between) / idle as f32;
            let per_busy = hooks / busy as f32;
            let ratio = per_idle / per_busy;
            let f = frames as f32;
            tries += &format!(
                "\n  {ratio:.2}: an idle instance {:.3} us, a busy one {:.3} us a frame; per frame scriptsSeed {:.3} ms, \
                 scriptsUnnamed {:.3}, scriptsHookless {:.3}, scriptsBetweenHooks {:.3}",
                per_idle / f * 1e3,
                per_busy / f * 1e3,
                seed / f,
                unnamed / f,
                hookless / f,
                between / f,
            );
            if ratio < bound {
                passed = true;
                break;
            }
        }
        assert!(passed, "2,000 idle `{kind}` instances cost more than {bound} of a busy one outside the hooks:{tries}");
    }
    crate::env::NO_CROSS_CHECK.with(|c| c.set(false));
    let _ = std::fs::remove_dir_all(&dir);
}

/// **What the seed step costs over an idle crowd**, printed rather than
/// held to a number: 230 instances that change nothing, as 46 kinds, as 46
/// kinds beside one script that mentions `getfenv` (which makes every
/// instance's `node` suspect), and as 230 kinds. Run with `--ignored
/// --nocapture`; the scripts folder is `SEED_BENCH_DIR` if set, so the disk
/// the project lives on can be measured.
#[test]
#[ignore]
fn seed_step_cost_over_an_idle_crowd() {
    let base = std::env::var("SEED_BENCH_DIR").map(std::path::PathBuf::from).unwrap_or_else(|_| std::env::temp_dir());
    for (kinds, extra) in [(46usize, ""), (46, "-- getfenv\n"), (230, "")] {
        let dir = base.join(format!("floptle-seed-bench-{}-{kinds}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for k in 0..kinds {
            let body = format!(
                "defaults = {{ speed = 1 }}\nfunction update(node, dt) end\n{}",
                if k == 0 { extra } else { "" }
            );
            write_script(&dir, &format!("k{k}"), &body);
        }
        let mut world = World::default();
        for i in 0..230usize {
            let e = world.spawn();
            world.insert(e, Transform::from_translation(glam::DVec3::new(i as f64, 0.0, 0.0)));
            world.insert(
                e,
                Scripts(vec![floptle_core::ScriptInst {
                    kind: format!("k{}", i % kinds),
                    enabled: true,
                    params: vec![("speed".into(), 2.0)],
                    refs: Vec::new(),
                    strs: Vec::new(),
                    pinned_bools: None,
                }]),
            );
        }
        let mut host = ScriptHost::new();
        host.profile().borrow_mut().enable(true);
        for f in 0..10 {
            profiled_frame(&mut host, &mut world, &dir, f);
        }
        assert!(host.errors().is_empty(), "{:?}", host.errors());
        let n = 600;
        let mut seed = 0.0f32;
        for f in 10..10 + n {
            seed += profiled_frame(&mut host, &mut world, &dir, f).part("scriptsSeed");
        }
        floptle_say::say_err!(
            "scriptsSeed over 230 idle instances, {kinds} kinds{}: {:.4} ms a frame",
            if extra.is_empty() { "" } else { ", beside a getfenv" },
            seed / n as f32
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The kept-table bench's scene: 230 `idle` instances (`starter`, whose only
/// hook is `start`, in the bench) and 50 `busy` ones, interleaved.
fn kept_tables_world(idle: &str) -> World {
    let mut world = World::default();
    for i in 0..280usize {
        let e = world.spawn();
        world.insert(e, Transform::from_translation(glam::DVec3::new(i as f64, 0.0, 0.0)));
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: if i % 28 < 23 { idle.into() } else { "busy".into() },
                enabled: true,
                params: vec![("speed".into(), 2.0)],
                refs: Vec::new(),
                strs: Vec::new(),
                pinned_bools: None,
            }]),
        );
    }
    world
}

/// **What keeping a handed `node` table current costs at rest**, printed:
/// 230 instances whose only hook is `start` (so every later pass is one
/// without a hook, checking the table it was handed), beside 50 running an
/// `update` interleaved with them, over the frame pass, a tick and the late
/// pass, checking each table field by field as before tables were armed and
/// then armed first. Run with `--ignored --nocapture`.
#[test]
#[ignore]
fn kept_node_tables_at_rest_cost() {
    let dir = std::env::temp_dir().join(format!("floptle-perf-kept-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_script(&dir, "busy", "function update(node, dt) node.y = node.y + dt * params.speed end\n");
    write_script(&dir, "starter", "defaults = { speed = 1 }\nfunction start(node) node.x = node.x + 0 end\n");
    // Both ways of checking a table, in the same run: read field by field,
    // as before tables were armed, then armed first. Without re-reading
    // every armed answer, as tests otherwise do.
    crate::env::NO_CROSS_CHECK.with(|c| c.set(true));
    for (way, chunk_only) in [("read field by field", true), ("armed first", false)] {
        crate::env::CHUNK_ONLY.with(|c| c.set(chunk_only));
        let mut world = kept_tables_world("starter");
        let mut host = ScriptHost::new();
        host.profile().borrow_mut().enable(true);
        for f in 0..10 {
            profiled_frame(&mut host, &mut world, &dir, f);
        }
        assert!(host.errors().is_empty(), "{:?}", host.errors());
        let n = 600;
        let (mut hookless, mut between) = (0.0f32, 0.0f32);
        crate::host::STAMP_CHECK_NS.with(|c| c.set(0));
        crate::host::STAMP_CHECK_TIMED.with(|c| c.set(true));
        crate::host::STAMPS_SKIPPED.with(|c| c.set(0));
        crate::host::STAMPS_SKIPPED_EARLY.with(|c| c.set(0));
        for f in 10..10 + n {
            let r = profiled_frame(&mut host, &mut world, &dir, f);
            hookless += r.part("scriptsHookless");
            between += r.part("scriptsBetweenHooks");
        }
        crate::host::STAMP_CHECK_TIMED.with(|c| c.set(false));
        let check = crate::host::STAMP_CHECK_NS.with(|c| c.get()) as f32 / 1e6;
        let f = n as f32;
        let skipped = crate::host::STAMPS_SKIPPED.with(|c| c.get()) as f32 / f;
        let early = crate::host::STAMPS_SKIPPED_EARLY.with(|c| c.get()) as f32 / f;
        floptle_say::say_err!(
            "230 kept tables at rest beside 50 busy, {way}, a frame: scriptsHookless {:.4} ms (the check's Lua call {:.4}, \
             the rest {:.4}), scriptsBetweenHooks {:.4} ms; {skipped:.0} stamps skipped, {early:.0} of them before any refresh",
            hookless / f,
            check / f,
            (hookless - check) / f,
            between / f,
        );
    }
    crate::env::CHUNK_ONLY.with(|c| c.set(false));
    crate::env::NO_CROSS_CHECK.with(|c| c.set(false));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The script pass's time outside every hook (`scriptsHookless` plus
/// `scriptsBetweenHooks`) a frame, over 300 frames of [`kept_tables_world`]
/// with `idle` as the idle script.
fn outside_hooks_ms(dir: &Path, idle: &str, chunk_only: bool) -> f32 {
    crate::env::CHUNK_ONLY.with(|c| c.set(chunk_only));
    let mut world = kept_tables_world(idle);
    let mut host = ScriptHost::new();
    host.profile().borrow_mut().enable(true);
    for f in 0..10 {
        profiled_frame(&mut host, &mut world, dir, f);
    }
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    let n = 300;
    let mut ms = 0.0f32;
    for f in 10..10 + n {
        let r = profiled_frame(&mut host, &mut world, dir, f);
        ms += r.part("scriptsHookless") + r.part("scriptsBetweenHooks");
    }
    crate::env::CHUNK_ONLY.with(|c| c.set(false));
    ms / n as f32
}

/// **A kept `node` table at rest costs next to nothing to check.** 230
/// instances whose only hook is `start` beside 50 busy ones, over the frame
/// pass, a tick and the late pass: the pass's time outside the hooks, less
/// the same for 230 hookless library instances (which hold no table), with
/// kept tables armed must come to at most 15% of what reading each table
/// field by field cost, measured in the same run (best of three, so a burst
/// of load on either side does not decide it).
///
/// Reading the table was about 0.8 us a table a pass. Asking its armed flag
/// instead, but only once a refresh had started, still paid the engine's own
/// bookkeeping around the check (a profile span, the memory owner, the
/// budget clock, three instance lookups and a registry fetch): 0.23 to 0.25
/// of the old cost in a test build (0.20 to 0.26 optimised), which this
/// bound fails. Asked before any of that, as `hookless_here` does, it is the
/// instance lookup the pass makes anyway, a body lookup and one byte: 0.09
/// in a test build and 0.03 to 0.04 optimised when this was written, about
/// 25 ns a table a pass against 700 ns for the read.
#[test]
fn a_kept_node_table_at_rest_costs_next_to_nothing_to_check() {
    let dir = std::env::temp_dir().join(format!("floptle-perf-armed-rest-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_script(&dir, "busy", "function update(node, dt) node.y = node.y + dt * params.speed end\n");
    write_script(&dir, "starter", "defaults = { speed = 1 }\nfunction start(node) node.x = node.x + 0 end\n");
    write_script(&dir, "lib", "defaults = { speed = 1 }\nlocal count = 0\nfunction bump() count = count + 1 return count end\n");
    crate::env::NO_CROSS_CHECK.with(|c| c.set(true));
    let mut tries = String::new();
    let mut passed = false;
    for _ in 0..3 {
        let base = outside_hooks_ms(&dir, "lib", false);
        let read = outside_hooks_ms(&dir, "starter", true) - base;
        let armed = outside_hooks_ms(&dir, "starter", false) - base;
        let ratio = armed / read;
        tries += &format!(
            "\n  {ratio:.3}: armed {armed:.4} ms, read field by field {read:.4} ms a frame over the hookless baseline {base:.4}"
        );
        if ratio <= 0.15 {
            passed = true;
            break;
        }
    }
    crate::env::NO_CROSS_CHECK.with(|c| c.set(false));
    let _ = std::fs::remove_dir_all(&dir);
    floptle_say::say_err!("230 kept tables at rest, outside the hooks:{tries}");
    assert!(passed, "checking 230 armed tables at rest cost more than 15% of reading them:{tries}");
}

/// A busy script's hook time per frame, best of `rounds`, with kept tables
/// armed (`chunk_only` false) or read field by field as before: 20
/// instances of `src` (kind `rw`), 30 frames after 5 to warm up.
fn busy_hook_ms(dir: &Path, src: &str, chunk_only: bool, rounds: usize) -> f32 {
    write_script(dir, "rw", src);
    let mut best = f32::INFINITY;
    for _ in 0..rounds {
        crate::env::CHUNK_ONLY.with(|c| c.set(chunk_only));
        let mut world = scripted_crowd("rw", 20, 0);
        let mut host = ScriptHost::new();
        host.profile().borrow_mut().enable(true);
        for f in 0..5 {
            profiled_frame(&mut host, &mut world, dir, f);
        }
        assert!(host.errors().is_empty(), "{:?}", host.errors());
        let mut ms = 0.0f32;
        for f in 5..35 {
            let r = profiled_frame(&mut host, &mut world, dir, f);
            ms += r.scripts.iter().filter(|(n, _)| n == "rw").map(|(_, ms)| *ms).sum::<f32>();
        }
        crate::env::CHUNK_ONLY.with(|c| c.set(false));
        best = best.min(ms / 30.0);
    }
    best
}

const BUSY_READS: &str = "function update(node, dt)\n  local s = 0\n  for i = 1, 2000 do s = s + node.x + node.y + node.z end\n  sum = s\nend\n";
const BUSY_WRITES: &str = "function update(node, dt)\n  local x = node.x\n  for i = 1, 2000 do node.x = x + i * 1e-9 end\n  node.x = x\nend\n";

/// **Arming a kept `node` table costs a busy script nothing.** A script
/// reading its `node` six thousand times an `update`, and one writing it two
/// thousand times, timed with kept tables armed and with them read field by
/// field as before, in the same run (best of five each, alternating, so a
/// burst of load lands on both): reads within 1.05 of the old cost, writes
/// within 1.10. An armed table reads through the same direct fields and
/// inline caches as any table; only the first write after each stamp takes
/// Luau's slow path, once.
///
/// And the measurement has teeth: the same reads through a ten-line proxy
/// table around `node` (the design this one was chosen over) must come out
/// well over the bound.
#[test]
fn an_armed_node_table_costs_a_busy_script_nothing() {
    let dir = std::env::temp_dir().join(format!("floptle-perf-armed-rw-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    crate::env::NO_CROSS_CHECK.with(|c| c.set(true));
    let proxy = "function update(node, dt)\n\
        local p = setmetatable({}, { __index = node, __newindex = function(_, k, v) node[k] = v end })\n\
        local s = 0\n  for i = 1, 2000 do s = s + p.x + p.y + p.z end\n  sum = s\nend\n";
    let mut report = String::new();
    let mut wrong = Vec::new();
    for (what, src, bound) in [("reads", BUSY_READS, 1.05f32), ("writes", BUSY_WRITES, 1.10)] {
        let (mut armed, mut read) = (f32::INFINITY, f32::INFINITY);
        for _ in 0..5 {
            armed = armed.min(busy_hook_ms(&dir, src, false, 1));
            read = read.min(busy_hook_ms(&dir, src, true, 1));
        }
        let ratio = armed / read;
        report += &format!("\n  {what}: armed {armed:.4} ms, read field by field {read:.4} ms a frame, {ratio:.3}");
        if ratio > bound {
            wrong.push(format!("{what} cost {ratio:.3} of the old cost, over {bound}"));
        }
    }
    let plain = busy_hook_ms(&dir, BUSY_READS, true, 3);
    let proxied = busy_hook_ms(&dir, proxy, false, 3);
    let teeth = proxied / plain;
    report += &format!("\n  reads through a proxy: {proxied:.4} ms against {plain:.4}, {teeth:.2}");
    if teeth < 1.5 {
        wrong.push(format!("reads through a proxy cost only {teeth:.2} of plain reads, so this measures nothing"));
    }
    crate::env::NO_CROSS_CHECK.with(|c| c.set(false));
    let _ = std::fs::remove_dir_all(&dir);
    floptle_say::say_err!("busy scripts with armed node tables:{report}");
    assert!(wrong.is_empty(), "{}:{report}", wrong.join("; "));
}

/// **`perf.sampleLimit` raises the sampler's line limit and says what it
/// is.** Set, it reads back; asked with nothing, it answers; a limit of zero
/// lines is refused rather than taken.
#[test]
fn perf_sample_limit_sets_and_reads_the_line_limit() {
    let host = ScriptHost::new();
    let (set, read, def): (i64, i64, i64) = host
        .lua
        .load("perf.enable(true) local d = perf.sampleLimit() return perf.sampleLimit(2000), perf.sampleLimit(), d")
        .eval()
        .unwrap();
    assert_eq!((set, read), (2000, 2000));
    assert_eq!(def as usize, floptle_core::profile::MAX_SAMPLE_LINES);
    assert!(host.lua.load("perf.sampleLimit(0)").exec().is_err(), "a limit of no lines was taken");
}
